//! Host-selected effort is durable session state, captured before token counting.
use crate::{
    api::SessionHandle,
    core::*,
    providers::{ReasoningOptions, validate_reasoning_tier},
};
use serde_json::{Value, json};
use std::time::Duration;

const KEY: &str = "_werkworker_reasoning";

pub(crate) fn selected(data: &SessionData) -> Option<String> {
    data.metadata[KEY]["tier"].as_str().map(str::to_owned)
}

impl SessionHandle {
    pub(crate) async fn model_context_snapshot(&self) -> Result<SessionData> {
        let mut data = self.snapshot().await?;
        // Existing children follow the root's choice on their next decision.
        // A request already planned or streaming retains its captured setting.
        if let Some(parent) = &data.parent {
            let root = self
                .engine
                .open_session(&parent.root_session)
                .await?
                .snapshot()
                .await?;
            if !data.metadata.is_object() {
                data.metadata = json!({});
            }
            data.metadata[KEY] = root.metadata.get(KEY).cloned().unwrap_or(Value::Null);
        }
        if data.metadata[KEY]["provider"].as_str() != Some(&self.engine.inner.provider.identity())
            && let Some(metadata) = data.metadata.as_object_mut()
        {
            metadata.remove(KEY);
        }
        Ok(data)
    }

    /// Query verified model tiers for the active run's reserve, or the supplied
    /// limits when idle. This performs only protocol discovery, never inference.
    pub async fn reasoning_options(&self, limits: &Limits) -> Result<ReasoningOptions> {
        let data = self.model_context_snapshot().await?;
        let effective = if let Some(run) = data.runs.iter().rev().find(|r| !r.status.terminal()) {
            crate::context::generation_limits(
                &run.limits,
                &run.generation_recovery,
                self.engine.inner.provider.as_ref(),
            )?
        } else {
            crate::context::effective_limits(
                limits,
                self.engine.inner.provider.capabilities().context_tokens,
            )?
        };
        let output = effective
            .reserve_output_tokens
            .min(crate::context::output_ceiling(
                &effective,
                self.engine.inner.provider.as_ref(),
            ));
        let mut options = tokio::time::timeout(
            Duration::from_secs(5),
            self.engine.inner.provider.reasoning_options(output),
        )
        .await
        .map_err(|_| WorkerError::new("reasoning_timeout", "Reasoning profile timed out"))??
        .ok_or_else(|| {
            WorkerError::new(
                "reasoning_unavailable",
                "This connection does not provide reasoning controls",
            )
        })?;
        if let Some(tier) = selected(&data) {
            options.requested = tier;
            options.native_effort = None;
        }
        Ok(options)
    }

    /// Applies to newly planned requests in this root session and its children.
    /// Revalidate availability before persisting; never silently coerce a tier.
    pub async fn set_reasoning_tier(
        &self,
        tier: &str,
        limits: &Limits,
    ) -> Result<ReasoningOptions> {
        validate_reasoning_tier(&json!(tier))?;
        if self.snapshot().await?.parent.is_some() {
            return Err(WorkerError::new(
                "reasoning_scope",
                "Select effort on the root session",
            ));
        }
        let mut options = self.reasoning_options(limits).await?;
        if tier != "default"
            && !options
                .profile
                .as_ref()
                .is_some_and(|p| p.supported && p.tiers.iter().any(|t| t.id == tier && t.available))
        {
            return Err(WorkerError::new(
                "invalid_reasoning_tier",
                format!(
                    "Reasoning tier '{tier}' is unavailable for this model and output limit. Select default or an available tier."
                ),
            ));
        }
        self.update(|data| {
            if data.metadata.is_null() {
                data.metadata = json!({});
            }
            if !data.metadata.is_object() {
                return Err(WorkerError::new(
                    "invalid_metadata",
                    "Session metadata must be an object to save reasoning controls",
                ));
            }
            data.metadata[KEY] =
                json!({"provider":self.engine.inner.provider.identity(),"tier":tier});
            data.emit(
                None,
                EventKind::Diagnostic {
                    code: format!("reasoning_tier_{tier}"),
                },
            );
            Ok(())
        })
        .await?;
        options.requested = tier.into();
        options.native_effort = None;
        Ok(options)
    }
}
