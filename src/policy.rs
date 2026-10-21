//! Host-owned capability allocation. Profile/skill contents never grant authority.
use crate::core::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Explicit host choices for a tool, independent of profile membership.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermission {
    Deny,
    Ask,
    Allow,
}

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
    /// without a preconfigured grant. FullAccess alone cannot skip this;
    /// explicit tool rules or remembered host approvals may override it.
    pub request_approvals: bool,
    pub tool_permissions: BTreeMap<String, ToolPermission>,
    /// User-approved scopes remembered by the host, including in request mode.
    pub remembered_grants: Vec<CapabilityGrant>,
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
/// The sandbox mount names the same confined workspace as a relative tool path.
/// This is a lexical alias only; traversal and arbitrary absolute paths stay denied.
pub fn workspace_path(path: &str) -> Result<&str> {
    let relative = if path == "/workspace" || path == "/workspace/" {
        "."
    } else {
        path.strip_prefix("/workspace/").unwrap_or(path)
    };
    relative_path(relative)?;
    Ok(relative)
}
pub fn resource(def: &ToolDefinition, args: &Value) -> Result<String> {
    match def.effect {
        Effect::Internal => Ok("internal".into()),
        Effect::Process => Ok("workspace".into()),
        Effect::Read | Effect::Write => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            let path = workspace_path(path)?;
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
    /// Omit tools with no requestable scope from the model's catalogue.
    /// Concrete arguments are still evaluated before preparation/execution.
    pub fn can_request(&self, def: &ToolDefinition) -> bool {
        if self.hard_denied_tools.contains(&def.name)
            || self.tool_permissions.get(&def.name) == Some(&ToolPermission::Deny)
            || def.effect == Effect::Write && !self.workspace_writable
        {
            return false;
        }
        def.effect == Effect::Internal
            || self.tool_permissions.contains_key(&def.name)
            || self.request_approvals
            || self.grants.iter().chain(&self.remembered_grants).any(|g| {
                g.tool == def.name
                    && g.version == def.version
                    && g.expires_at_ms.is_none_or(|t| t > now_ms())
            })
    }

    pub fn evaluate(
        &self,
        def: &ToolDefinition,
        args: &Value,
        similar: &[String],
    ) -> PolicyDecision {
        if self.hard_denied_tools.contains(&def.name) {
            return PolicyDecision::Deny;
        }
        let permission = self.tool_permissions.get(&def.name);
        if permission == Some(&ToolPermission::Deny) {
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
        if self.remembered_grants.iter().any(|g| {
            g.tool == def.name
                && g.version == def.version
                && g.expires_at_ms.is_none_or(|t| t > now_ms())
                && g.arguments_hash
                    .as_ref()
                    .is_none_or(|h| h == &fingerprint(args))
                && (g.resource == target
                    || matches!(def.effect, Effect::Read | Effect::Write) && g.resource == ".")
        }) {
            return PolicyDecision::Allow;
        }
        match permission {
            Some(ToolPermission::Allow) => return PolicyDecision::Allow,
            Some(ToolPermission::Ask) => return PolicyDecision::Ask,
            _ => {}
        }
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
