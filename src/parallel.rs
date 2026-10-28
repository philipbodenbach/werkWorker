//! Host-owned admission limits and cooperative workspace access.
use crate::{core::*, providers::InferenceConcurrency};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Delegation strategy, independent of host capacity and tool permissions.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DelegationLevel {
    Off,
    Conservative,
    #[default]
    Active,
    Max,
}

impl DelegationLevel {
    pub const ALL: [Self; 4] = [Self::Off, Self::Conservative, Self::Active, Self::Max];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Conservative => "Conservative",
            Self::Active => "Active",
            Self::Max => "Max",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Off => {
                "No new subagents, including automatic reviews. Existing children continue; status, messages and cancellation remain available."
            }
            Self::Conservative => {
                "Delegate a focused independent task when useful. One automatic read-only review after an implementation change and a passing behavioral check."
            }
            Self::Active => {
                "Prefer delegating independent investigations and reviews while implementing. One automatic read-only review after the first implementation change, before verification. This is the default."
            }
            Self::Max => {
                "Proactively split substantial work into independent, bounded tasks early. One automatic read-only requirements/check-planning helper can start once a substantial plan exists. Use available helper capacity when useful, without duplicating work or inventing tasks."
            }
        }
    }

    pub fn from_metadata(metadata: &serde_json::Value) -> Result<Self> {
        Ok(Self::project_override(metadata)?.unwrap_or(Self::preference(metadata)?))
    }

    /// Explicit project configuration always takes precedence over UI preferences.
    pub fn project_override(metadata: &serde_json::Value) -> Result<Option<Self>> {
        Self::parse(
            metadata
                .get("project_config")
                .and_then(|c| c.get("delegation_level")),
        )
    }

    pub fn preference(metadata: &serde_json::Value) -> Result<Self> {
        Ok(Self::parse(metadata.get(PREFERENCE_KEY))?.unwrap_or_default())
    }

    fn parse(value: Option<&serde_json::Value>) -> Result<Option<Self>> {
        match value {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(value) => serde_json::from_value(value.clone())
                .map(Some)
                .map_err(|_| {
                    WorkerError::new(
                        "delegation_level",
                        "Delegation level must be off, conservative, active or max",
                    )
                }),
        }
    }

    fn helper_ceiling(self) -> usize {
        match self {
            Self::Off => 0,
            Self::Conservative => 2,
            Self::Active => 4,
            Self::Max => usize::MAX,
        }
    }
}

pub const PREFERENCE_KEY: &str = "_werkworker_delegation_preference";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionConfig {
    pub max_model_requests: usize,
    pub max_subagents: usize,
    pub max_children_per_run: usize,
    pub max_depth: usize,
}
impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            max_model_requests: 4,
            max_subagents: 4,
            max_children_per_run: 8,
            max_depth: 1,
        }
    }
}
impl ExecutionConfig {
    pub fn validate(&self) -> Result<()> {
        if !(1..=64).contains(&self.max_model_requests)
            || self.max_subagents > 32
            || self.max_children_per_run > 64
            || self.max_depth > 4
        {
            return Err(WorkerError::new(
                "invalid_limits",
                "Invalid inference/subagent limits",
            ));
        }
        Ok(())
    }
    pub fn effective_requests(&self, info: &InferenceConcurrency) -> usize {
        self.max_model_requests
            .min(info.max_requests.unwrap_or(1).max(1))
    }
}

/// Give the model the host/provider ceiling without a status-only inference
/// turn. Avoid transient busy counters in tool descriptions: those would churn
/// the shared prompt prefix on every admission. Actual admission stays enforced
/// by the scheduler; this is guidance, not a reservation or additional grant.
pub(crate) fn describe_delegation(
    tools: &mut Vec<ToolDefinition>,
    config: &ExecutionConfig,
    info: &InferenceConcurrency,
    depth: usize,
    level: DelegationLevel,
) {
    if level == DelegationLevel::Off {
        tools.retain(|tool| tool.name != "delegate_task");
        return;
    }
    let Some(tool) = tools.iter_mut().find(|tool| tool.name == "delegate_task") else {
        return;
    };
    let slots = config.effective_requests(info);
    let helpers = if depth < config.max_depth {
        slots
            .saturating_sub(1)
            .min(config.max_subagents)
            .min(config.max_children_per_run)
            .min(level.helper_ceiling())
    } else {
        0
    };
    tool.description.push_str(&format!(
        " Delegation level: {}. {} Effective configured inference slots: {slots}. Recommended total active helpers for this parent: at most {helpers}, including already running automatic reviews. Configured slots are shared capacity, not reserved throughput.", level.label(), level.description()
    ));
    if helpers > 0 {
        tool.description.push_str(
            " For substantial work, delegate a focused independent investigation or review when it removes work from your own path. Give exact relevant paths, requirements, and a concise expected result; prefer read_only for reviews. Continue your separate implementation while the helper works. Avoid duplicating its investigation, overlapping writes, immediate agents.wait, or repeated status polling; completed results arrive automatically. For small or sequential work, work directly."
        );
    } else {
        tool.description.push_str(
            " No parallel helper is recommended under the current capacity/depth limits. Work directly unless explicit delegation is needed for isolation; it may execute serially and still obeys host child limits."
        );
    }
}

/// A lowered capacity prevents new admissions until existing requests drain;
/// raising it wakes waiters without cancelling any in-flight inference.
pub(crate) struct InferenceSlots {
    state: Mutex<(usize, usize)>,
    changed: tokio::sync::Notify,
}
pub(crate) struct InferencePermit(Arc<InferenceSlots>);
impl InferenceSlots {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((0, limit.max(1))),
            changed: tokio::sync::Notify::new(),
        })
    }
    pub fn set_limit(&self, limit: usize) {
        self.state.lock().unwrap().1 = limit.max(1);
        self.changed.notify_waiters();
    }
    pub fn available_permits(&self) -> usize {
        let (active, limit) = *self.state.lock().unwrap();
        limit.saturating_sub(active)
    }
    pub async fn acquire(self: &Arc<Self>) -> InferencePermit {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.state.lock().unwrap();
                if state.0 < state.1 {
                    state.0 += 1;
                    return InferencePermit(self.clone());
                }
            }
            changed.await;
        }
    }
}
impl Drop for InferencePermit {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().0 -= 1;
        self.0.changed.notify_waiters();
    }
}
#[derive(Default)]
pub(crate) struct WorkspaceAccess {
    leases: Mutex<Vec<(String, PathBuf, bool)>>,
}
pub struct WorkspaceLease {
    id: String,
    access: Arc<WorkspaceAccess>,
}
impl WorkspaceAccess {
    pub fn try_acquire(self: &Arc<Self>, path: &Path, write: bool) -> Option<Arc<WorkspaceLease>> {
        let mut leases = self.leases.lock().unwrap();
        if leases.iter().any(|(_, other, exclusive)| {
            (write || *exclusive) && (path.starts_with(other) || other.starts_with(path))
        }) {
            return None;
        }
        let id = id("lease");
        leases.push((id.clone(), path.to_owned(), write));
        Some(Arc::new(WorkspaceLease {
            id,
            access: self.clone(),
        }))
    }
}
impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        self.access
            .leases
            .lock()
            .unwrap()
            .retain(|(id, _, _)| *id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlapping_workspaces_share_reads_and_exclude_writers_until_last_lease_drops() {
        let access = Arc::new(WorkspaceAccess::default());
        let path = Path::new("workspace");
        let read = access.try_acquire(path, false).unwrap();
        let retained_by_job = read.clone();
        let nested = access.try_acquire(&path.join("child"), false).unwrap();
        assert!(access.try_acquire(path, true).is_none());
        assert!(access.try_acquire(Path::new("independent"), true).is_some());
        drop(read);
        drop(nested);
        assert!(access.try_acquire(path, true).is_none());
        drop(retained_by_job);
        let write = access.try_acquire(path, true).unwrap();
        assert!(access.try_acquire(&path.join("child"), false).is_none());
        drop(write);
        assert!(access.try_acquire(path, false).is_some());
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use crate::tools::Tool;

    #[test]
    fn delegation_guidance_respects_provider_host_and_depth_limits() {
        for (capacity, host, children, depth, expected) in [
            (None, 4, 4, 0, 0),
            (Some(2), 4, 4, 0, 1),
            (Some(8), 4, 4, 0, 2),
            (Some(8), 1, 4, 0, 0),
            (Some(8), 4, 0, 0, 0),
            (Some(8), 4, 4, 1, 0),
        ] {
            let config = ExecutionConfig {
                max_model_requests: host,
                max_subagents: children,
                ..ExecutionConfig::default()
            };
            let original = crate::agents::AgentTool("delegate_task".into()).definition();
            let mut tools = vec![original.clone()];
            describe_delegation(
                &mut tools,
                &config,
                &InferenceConcurrency {
                    max_requests: capacity,
                    source: "fixture".into(),
                    detail: "test".into(),
                },
                depth,
                DelegationLevel::Conservative,
            );
            assert!(tools[0].description.contains(&format!(
                "Recommended total active helpers for this parent: at most {expected}"
            )));
            assert_eq!(tools[0].input_schema, original.input_schema);
            assert_eq!(tools[0].effect, original.effect);
        }
    }

    #[test]
    fn levels_change_guidance_within_host_capacity_and_off_keeps_controls() {
        let config = ExecutionConfig {
            max_model_requests: 8,
            max_subagents: 6,
            ..Default::default()
        };
        let info = InferenceConcurrency {
            max_requests: Some(8),
            source: "fixture".into(),
            detail: "test".into(),
        };
        for (level, expected) in [
            (DelegationLevel::Conservative, 2),
            (DelegationLevel::Active, 4),
            (DelegationLevel::Max, 6),
        ] {
            let mut tools = vec![crate::agents::AgentTool("delegate_task".into()).definition()];
            describe_delegation(&mut tools, &config, &info, 0, level);
            assert!(
                tools[0]
                    .description
                    .contains(&format!("at most {expected},"))
            );
            assert!(tools[0].description.contains(level.description()));
        }
        let mut tools = vec![
            crate::agents::AgentTool("delegate_task".into()).definition(),
            crate::agents::AgentTool("agents.cancel".into()).definition(),
        ];
        describe_delegation(&mut tools, &config, &info, 0, DelegationLevel::Off);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "agents.cancel");
    }

    #[tokio::test]
    async fn capacity_changes_drain_inflight_requests_and_wake_waiters() {
        let slots = InferenceSlots::new(2);
        let first = slots.acquire().await;
        let second = slots.acquire().await;
        slots.set_limit(1);
        drop(first);
        assert_eq!(slots.available_permits(), 0);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), slots.acquire())
                .await
                .is_err()
        );
        drop(second);
        let first = slots.acquire().await;
        slots.set_limit(2);
        let second = tokio::time::timeout(std::time::Duration::from_millis(100), slots.acquire())
            .await
            .unwrap();
        drop((first, second));
        assert_eq!(slots.available_permits(), 2);
    }
}
