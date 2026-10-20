//! Host-owned capability allocation. Profile/skill contents never grant authority.
use crate::core::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    #[default]
    Ask,
    FullAccess,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityGrant {
    pub tool: String,
    pub version: String,
    /// Path subtree for files, exact origin for HTTP, "workspace" for processes.
    pub resource: String,
    /// Optional exact argv/arguments restriction; never a shell prefix.
    pub arguments_hash: Option<String>,
    pub expires_at_ms: Option<u64>,
    /// Explicit host opt-in to skip individual approval in this exact scope.
    #[serde(default)]
    pub preapproved: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct HostPolicy {
    pub mode: ApprovalMode,
    pub grants: Vec<CapabilityGrant>,
    pub hard_denied_tools: Vec<String>,
    pub allow_similar: bool,
    pub workspace_writable: bool,
    /// Trusted host opt-in: valid tool actions may request explicit approval
    /// without a preconfigured grant. Never skips approval, including FullAccess.
    pub request_approvals: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny,
}
pub fn relative_path(path: &str) -> Result<()> {
    use std::path::{Component, Path};
    if path.is_empty()
        || path.len() > 4096
        || path.contains('\\')
        || path.contains('\0')
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(WorkerError::new(
            "path_denied",
            "Expected a relative workspace path without traversal",
        ));
    }
    Ok(())
}
pub fn resource(def: &ToolDefinition, args: &Value) -> Result<String> {
    match def.effect {
        Effect::Internal => Ok("internal".into()),
        Effect::Process => Ok("workspace".into()),
        Effect::Read | Effect::Write => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            relative_path(path)?;
            Ok(path.trim_start_matches("./").to_string())
        }
        Effect::NetworkRead | Effect::ExternalWrite => {
            // Connectors supply a fixed origin in their prepared object.
            let origin = args
                .get("origin")
                .or_else(|| args.get("url"))
                .and_then(Value::as_str)
                .ok_or_else(|| WorkerError::new("invalid_resource", "Missing broker origin"))?;
            let url = reqwest::Url::parse(origin)
                .map_err(|_| WorkerError::new("invalid_url", "Invalid broker origin"))?;
            Ok(url.origin().ascii_serialization())
        }
    }
}
impl HostPolicy {
    pub fn evaluate(
        &self,
        def: &ToolDefinition,
        args: &Value,
        similar: &[String],
    ) -> PolicyDecision {
        if self.hard_denied_tools.contains(&def.name) {
            return PolicyDecision::Deny;
        }
        if def.effect == Effect::Internal {
            return PolicyDecision::Allow;
        }
        if def.effect == Effect::Write && !self.workspace_writable {
            return PolicyDecision::Deny;
        }
        let Ok(target) = resource(def, args) else {
            return PolicyDecision::Deny;
        };
        if self.request_approvals {
            return PolicyDecision::Ask;
        }
        let granted = self.grants.iter().find(|g| {
            g.tool == def.name
                && g.version == def.version
                && g.expires_at_ms.is_none_or(|t| t > now_ms())
                && g.arguments_hash
                    .as_ref()
                    .is_none_or(|h| h == &fingerprint(args))
                && match def.effect {
                    Effect::Read | Effect::Write => {
                        relative_path(&g.resource).is_ok()
                            && (g.resource == "."
                                || target == g.resource
                                || target
                                    .starts_with(&format!("{}/", g.resource.trim_end_matches('/'))))
                    }
                    _ => g.resource == target,
                }
        });
        let Some(grant) = granted else {
            return PolicyDecision::Deny;
        };
        if grant.preapproved
            || self.mode == ApprovalMode::FullAccess
            || similar.contains(&approval_key(def, args))
        {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Ask
        }
    }
}
pub fn approval_key(def: &ToolDefinition, args: &Value) -> String {
    fingerprint(&(def.name.clone(), def.version.clone(), args))
}
