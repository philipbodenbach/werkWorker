//! Interactive session configuration and explicitly saved workspace defaults.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use werk_worker::{
    parallel::DelegationLevel,
    policy::{HostPolicy, PolicyDecision, ToolPermission},
    profiles::AgentProfile,
    project::{Access, Project},
    *,
};

#[derive(Serialize, Deserialize)]
struct SavedPolicy {
    workspace_identity: String,
    policy: HostPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delegation_level: Option<DelegationLevel>,
}

async fn policy_path(workspace: &Path) -> Result<PathBuf> {
    let root = tokio::fs::canonicalize(workspace).await?;
    Ok(crate::cli_setup::settings_path()?
        .parent()
        .expect("config parent")
        .join("workspace-access")
        .join(format!("{}.json", fingerprint(&root))))
}

async fn load_saved(workspace: &Path) -> Result<Option<SavedPolicy>> {
    let bytes = match crate::cli_setup::read_settings(&policy_path(workspace).await?).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let saved: SavedPolicy = serde_json::from_slice(&bytes)?;
    let current = runtime::Workspace::open(workspace).await?;
    Ok((saved.workspace_identity == current.identity).then_some(saved))
}

pub async fn load_policy(workspace: &Path) -> Result<Option<HostPolicy>> {
    Ok(load_saved(workspace).await?.map(|saved| saved.policy))
}

pub async fn session_metadata(project: &Project, workspace: &Path) -> Result<serde_json::Value> {
    let mut metadata = project.scoped_metadata();
    if let Some(level) = load_saved(workspace)
        .await?
        .and_then(|s| s.delegation_level)
    {
        metadata[parallel::PREFERENCE_KEY] = serde_json::json!(level);
    }
    Ok(metadata)
}

pub async fn save_policy(workspace: &Path, policy: HostPolicy) -> Result<()> {
    let preference = load_saved(workspace)
        .await?
        .and_then(|s| s.delegation_level);
    save_preferences(workspace, policy, preference).await
}

async fn save_preferences(
    workspace: &Path,
    policy: HostPolicy,
    delegation_level: Option<DelegationLevel>,
) -> Result<()> {
    let current = runtime::Workspace::open(workspace).await?;
    crate::cli_setup::save(
        &policy_path(workspace).await?,
        &SavedPolicy {
            workspace_identity: current.identity,
            policy,
            delegation_level,
        },
    )
    .await
}

pub async fn save_defaults(session: &SessionHandle) -> Result<()> {
    let data = session.snapshot().await?;
    let project = Project::load(&data.workspace).await?;
    let mut config = project.config.clone();
    config.profile = Some(data.profile.name);
    config.tools = Some(data.profile.tools);
    config.skills = Some(data.profile.skills);
    project.save(&data.workspace, &config).await?;
    save_policy(&data.workspace, data.policy).await
}

pub async fn browser_command(session: &SessionHandle, command: &str) -> Result<String> {
    match command.trim() {
        "/browser on" => {
            session.set_browser_mode(true, &id("browser-on")).await?;
            Ok(format!(
                "Browser mode enabled. Open the panel for website permissions, approvals and manual control:\n{}",
                session.browser_panel().await?
            ))
        }
        "/browser off" => {
            session.set_browser_mode(false, &id("browser-off")).await?;
            Ok("Browser mode disabled for subsequent actions. Stop the active run to interrupt an action already in progress.".into())
        }
        "/browser" => Ok(format!(
            "/browser on · /browser off\nBrowser panel: {}",
            session.browser_panel().await?
        )),
        _ => Err(WorkerError::new(
            "browser_command",
            "Use /browser on, /browser off, or /browser to open the panel",
        )),
    }
}

pub const COMMAND_HELP: &str = "/permissions reads|writes|process deny|ask|allow [session|project]\n/permissions mode manual|edits|readonly [session|project]\nExamples: /permissions writes allow project · /permissions mode manual session\nDefault scope: session + project. Changes also enable the required tools.";

pub async fn permission_command(session: &SessionHandle, command: &str) -> Result<String> {
    let args: Vec<_> = command.split_whitespace().collect();
    if !(3..=4).contains(&args.len()) {
        return Err(WorkerError::new("permission_command", COMMAND_HELP));
    }
    let mut editor = Editor::open(session).await?;
    editor.save_project = match args.get(3).copied().unwrap_or("project") {
        "project" => true,
        "session" => false,
        _ => return Err(WorkerError::new("permission_command", COMMAND_HELP)),
    };
    if args[1] == "mode" && ["manual", "edits", "readonly"].contains(&args[2]) {
        editor.set_mode(args[2]);
    } else {
        let effect = match args[1] {
            "reads" => Effect::Read,
            "writes" => Effect::Write,
            "process" => Effect::Process,
            _ => return Err(WorkerError::new("permission_command", COMMAND_HELP)),
        };
        let permission = match args[2] {
            "deny" => ToolPermission::Deny,
            "ask" => ToolPermission::Ask,
            "allow" => ToolPermission::Allow,
            _ => return Err(WorkerError::new("permission_command", COMMAND_HELP)),
        };
        editor.set_group(effect, permission);
    }
    editor.apply(session).await?;
    Ok(format!("{command}\n{}", editor.status))
}

pub fn policy_label(policy: &HostPolicy) -> String {
    if !policy.tool_permissions.is_empty() || !policy.remembered_grants.is_empty() {
        return format!(
            "{} tool rules; {} remembered approvals; workspace writes {}",
            policy.tool_permissions.len(),
            policy.remembered_grants.len(),
            if policy.workspace_writable {
                "enabled"
            } else {
                "disabled"
            }
        );
    }
    if policy.request_approvals {
        format!(
            "Ask before each action; workspace writes {}",
            if policy.workspace_writable {
                "enabled"
            } else {
                "disabled"
            }
        )
    } else {
        format!(
            "Scoped grants: {}; workspace writes {}; mode {:?}",
            policy.grants.len(),
            if policy.workspace_writable {
                "enabled"
            } else {
                "disabled"
            },
            policy.mode
        )
    }
}

pub const TABS: [&str; 5] = ["Profiles", "Permissions", "Tools", "Skills", "Agents"];

pub struct Editor {
    pub profile: AgentProfile,
    pub policy: HostPolicy,
    pub delegation_level: DelegationLevel,
    project_delegation_level: Option<DelegationLevel>,
    pub tab: usize,
    pub selected: [usize; TABS.len()],
    pub save_project: bool,
    pub status: String,
    profiles: Vec<AgentProfile>,
    tools: Vec<ToolDefinition>,
    skills: Vec<skills::Skill>,
    project: Project,
    workspace: PathBuf,
    saved_draft: String,
    saved_access: String,
}

impl Editor {
    pub async fn open(session: &SessionHandle) -> Result<Self> {
        let data = session.snapshot().await?;
        let project = Project::load(&data.workspace).await?;
        let global = crate::cli_setup::settings_path()?
            .parent()
            .expect("config parent")
            .to_path_buf();
        // Start from each preset's tool/skill set; explicit permission choices
        // are retained when the user later activates a preset.
        let mut presets = project.clone();
        // Profile selection must not silently import changed on-disk rules into
        // an existing session. Scoped sessions retain their separate snapshots;
        // embedded legacy sessions keep the original snapshot text in presets.
        let scoped = data.metadata.get("_werkworker_project_context").is_some();
        if let Some(rules) = data.metadata.get("project_instructions") {
            presets.instructions = serde_json::from_value(rules.clone())?;
        } else {
            presets.instructions.clear();
        }
        presets.config.tools = None;
        presets.config.skills = None;
        let mut profiles = Vec::new();
        for name in Project::profile_names(&data.workspace, &global).await? {
            profiles.push(if scoped {
                presets
                    .scoped_profile(&name, &data.workspace, &global)
                    .await?
            } else {
                presets.profile(&name, &data.workspace, &global).await?
            });
        }
        if !profiles.iter().any(|p| p.name == data.profile.name) {
            profiles.push(data.profile.clone());
        }
        let selected = profiles
            .iter()
            .position(|p| p.name == data.profile.name)
            .unwrap_or(0);
        let delegation_level = DelegationLevel::preference(&data.metadata)?;
        let project_delegation_level = DelegationLevel::project_override(&data.metadata)?;
        let saved_access = fingerprint(&(&data.profile, &data.policy));
        let saved_draft = fingerprint(&(&data.profile, &data.policy, delegation_level));
        Ok(Self {
            profile: data.profile,
            policy: data.policy,
            delegation_level,
            project_delegation_level,
            tab: 1,
            selected: [
                selected,
                0,
                0,
                0,
                DelegationLevel::ALL
                    .iter()
                    .position(|l| *l == delegation_level)
                    .unwrap(),
            ],
            save_project: true,
            status: String::new(),
            profiles,
            tools: session.available_tools(),
            skills: session.available_skills(),
            project,
            workspace: data.workspace,
            saved_draft,
            saved_access,
        })
    }

    pub fn items(&self) -> Vec<String> {
        match self.tab {
            0 => self
                .profiles
                .iter()
                .map(|p| {
                    format!(
                        "[{}] {}",
                        if p.name == self.profile.name {
                            "x"
                        } else {
                            " "
                        },
                        p.name
                    )
                })
                .collect(),
            1 => vec![
                format!("Workspace reads: {}", self.group_label(Effect::Read)),
                format!("Workspace writes: {}", self.group_label(Effect::Write)),
                format!("Sandbox processes: {}", self.group_label(Effect::Process)),
                "Preset: Manual".into(),
                "Preset: Accept edits".into(),
                "Preset: Read only".into(),
                "Preset: Ask before every action (all tools)".into(),
                format!(
                    "Forget remembered approvals ({})",
                    self.policy.remembered_grants.len()
                ),
            ],
            2 => self
                .tools
                .iter()
                .map(|t| {
                    format!(
                        "[{}] {}{} · {}",
                        if self.profile.tools.contains(&t.name) {
                            "x"
                        } else {
                            " "
                        },
                        t.name,
                        if self.policy.hard_denied_tools.contains(&t.name) {
                            " (hard denied)"
                        } else {
                            ""
                        },
                        self.tool_label(t),
                    )
                })
                .collect(),
            3 => self
                .skills
                .iter()
                .map(|s| {
                    format!(
                        "[{}] {}",
                        if self.profile.skills.contains(&s.name) {
                            "x"
                        } else {
                            " "
                        },
                        s.name
                    )
                })
                .collect(),
            _ => DelegationLevel::ALL
                .iter()
                .map(|level| {
                    format!(
                        "[{}] {}",
                        if *level == self.delegation_level {
                            "x"
                        } else {
                            " "
                        },
                        level.label()
                    )
                })
                .collect(),
        }
    }

    pub fn move_selection(&mut self, up: bool) {
        let count = self.items().len();
        if count > 0 {
            let selected = &mut self.selected[self.tab];
            *selected = if up {
                (*selected + count - 1) % count
            } else {
                (*selected + 1) % count
            };
        }
    }

    pub fn activate(&mut self) {
        let selected = self.selected[self.tab];
        match self.tab {
            0 => {
                if let Some(profile) = self.profiles.get(selected) {
                    self.profile = profile.clone();
                    // Keep tool access the user explicitly enabled in this
                    // editor; a later profile choice must not silently undo it.
                    for (name, permission) in &self.policy.tool_permissions {
                        if *permission != ToolPermission::Deny && !self.profile.tools.contains(name)
                        {
                            self.profile.tools.push(name.clone());
                        }
                    }
                }
            }
            1 => {
                if selected == 6 {
                    let denied = self.policy.hard_denied_tools.clone();
                    self.policy = Access::Ask.policy();
                    self.policy.hard_denied_tools = denied;
                    for tool in &self.tools {
                        if (self.policy.request_approvals
                            || self.policy.grants.iter().any(|g| g.tool == tool.name))
                            && !self.profile.tools.contains(&tool.name)
                        {
                            self.profile.tools.push(tool.name.clone());
                        }
                    }
                } else if selected <= 2 {
                    let effect = [Effect::Read, Effect::Write, Effect::Process][selected];
                    let permission = match self.group_label(effect).as_str() {
                        "deny" | "disabled" | "mixed" => ToolPermission::Ask,
                        "ask" => ToolPermission::Allow,
                        _ => ToolPermission::Deny,
                    };
                    self.set_group(effect, permission);
                } else if (3..=5).contains(&selected) {
                    self.set_mode(["manual", "edits", "readonly"][selected - 3]);
                } else if selected == 7 {
                    self.policy.remembered_grants.clear();
                }
            }
            2 => {
                if let Some(tool) = self.tools.get(selected) {
                    toggle(&mut self.profile.tools, &tool.name);
                }
            }
            3 => {
                if let Some(skill) = self.skills.get(selected) {
                    toggle(&mut self.profile.skills, &skill.name);
                    if !self.profile.skills.is_empty() {
                        for name in ["skills.list", "skills.load"] {
                            if self.tools.iter().any(|t| t.name == name)
                                && !self.profile.tools.iter().any(|t| t == name)
                            {
                                self.profile.tools.push(name.into());
                            }
                        }
                    }
                }
            }
            _ => {
                if let Some(level) = DelegationLevel::ALL.get(selected) {
                    self.delegation_level = *level;
                }
            }
        }
        self.status =
            "Draft changed. Ctrl-S applies settings; Esc discards unsaved changes.".into();
    }

    pub fn details(&self) -> String {
        let selected = self.selected[self.tab];
        let detail = match self.tab {
            0 => self.profiles.get(selected).map(|p| format!("{}\n\nProfiles choose tools and skills. Change workspace writes and sandbox processes in the Permissions tab. Explicitly enabled tool permissions are retained when selecting a profile.\n\nTools: {}\nSkills: {}\n\n{}", p.name, p.tools.join(", "), p.skills.join(", "), p.prompt)),
            1 => Some("Workspace reads / writes / processes: Enter cycles deny → ask → allow. Enabling access also enables the matching tools in this profile.\n\nManual: reads run directly; writes and commands ask.\nAccept edits: workspace reads/writes run directly; commands ask.\nRead only: reads run directly; writes and commands are blocked.\n\nCtrl-S applies the draft. P switches session-only / project defaults. Saved approvals can be cleared here. Hard denies remain authoritative.".into()),
            2 => self.tools.get(selected).map(|t| format!("{}\nEffect: {:?}\nPermission: {}\n\nEnter: enable/disable tool\nA: cycle deny / ask / allow (whole tool)\n\n{}", t.name, t.effect, self.tool_label(t), t.description)),
            3 => self.skills.get(selected).map(|s| format!("{}\n{}\n\n{}", s.name, s.description, s.body)),
            _ => DelegationLevel::ALL.get(selected).map(|l| format!("Delegation preference: {}\n\n{}\n\nEnter selects a preference; Ctrl-S applies it. An explicit delegation_level in .werkworker/config.json always wins; settings never overwrite that value. Delegation-only changes also apply during a run. Existing children finish normally.\n\nAutomatic helpers require substantial coding work and at least two verified inference slots. Every level respects tool permissions, tree budgets and host capacity. More agents share the same model resources.\n\nP switches session-only / saved workspace preference.", l.label(), l.description())),
        }.unwrap_or_default();
        format!(
            "Draft profile: {}\n{}\nEnabled: {} tools, {} skills\nDelegation preference: {}\nEffective: {}\nSave: {}\n\n{}\n\n{}",
            self.profile.name,
            policy_label(&self.policy),
            self.profile.tools.len(),
            self.profile.skills.len(),
            self.delegation_level.label(),
            self.project_delegation_level
                .map(|level| format!("{} (fixed by project config)", level.label()))
                .unwrap_or_else(|| self.delegation_level.label().into()),
            if self.save_project {
                "current session + project defaults"
            } else {
                "current session only"
            },
            detail,
            self.status
        )
    }

    pub fn is_dirty(&self) -> bool {
        fingerprint(&(&self.profile, &self.policy, self.delegation_level)) != self.saved_draft
    }

    fn tool_label(&self, tool: &ToolDefinition) -> String {
        if self.policy.hard_denied_tools.contains(&tool.name) {
            return "hard denied".into();
        }
        if !self.profile.tools.contains(&tool.name) {
            return "disabled".into();
        }
        if tool.effect == Effect::Write && !self.policy.workspace_writable {
            return "deny".into();
        }
        if let Some(rule) = self.policy.tool_permissions.get(&tool.name) {
            return format!("{rule:?}").to_lowercase();
        }
        if self
            .policy
            .remembered_grants
            .iter()
            .any(|g| g.tool == tool.name)
        {
            return "remembered scope".into();
        }
        match self
            .policy
            .evaluate(tool, &serde_json::json!({"path": "."}), &[])
        {
            PolicyDecision::Allow => "allow",
            PolicyDecision::Ask => "ask",
            PolicyDecision::Deny => "deny",
        }
        .into()
    }

    fn group_label(&self, effect: Effect) -> String {
        if effect == Effect::Write && !self.policy.workspace_writable {
            return "deny".into();
        }
        let labels: std::collections::BTreeSet<_> = self
            .tools
            .iter()
            .filter(|t| t.effect == effect)
            .map(|t| self.tool_label(t))
            .collect();
        if labels.len() == 1 {
            labels.into_iter().next().unwrap()
        } else {
            "mixed".into()
        }
    }

    fn set_tool(&mut self, name: &str, permission: ToolPermission) {
        self.policy.remembered_grants.retain(|g| g.tool != name);
        self.policy.tool_permissions.insert(name.into(), permission);
        if permission != ToolPermission::Deny && !self.profile.tools.iter().any(|t| t == name) {
            self.profile.tools.push(name.into());
        }
    }

    pub fn set_group(&mut self, effect: Effect, permission: ToolPermission) {
        let names: Vec<_> = self
            .tools
            .iter()
            .filter(|t| t.effect == effect)
            .map(|t| t.name.clone())
            .collect();
        for name in names {
            self.set_tool(&name, permission);
        }
        if effect == Effect::Write {
            self.policy.workspace_writable = permission != ToolPermission::Deny;
        }
    }

    pub fn cycle_tool_permission(&mut self) {
        if self.tab != 2 {
            return;
        }
        if let Some(tool) = self.tools.get(self.selected[2]).cloned() {
            let permission = match self.tool_label(&tool).as_str() {
                "deny" | "disabled" => ToolPermission::Ask,
                "ask" => ToolPermission::Allow,
                _ => ToolPermission::Deny,
            };
            self.set_tool(&tool.name, permission);
            if tool.effect == Effect::Write && permission != ToolPermission::Deny {
                self.policy.workspace_writable = true;
            }
            self.status = "Draft permission changed. Ctrl-S saves.".into();
        }
    }

    pub fn set_mode(&mut self, mode: &str) {
        self.set_group(Effect::Read, ToolPermission::Allow);
        self.set_group(
            Effect::Write,
            match mode {
                "readonly" => ToolPermission::Deny,
                "edits" => ToolPermission::Allow,
                _ => ToolPermission::Ask,
            },
        );
        self.set_group(
            Effect::Process,
            if mode == "readonly" {
                ToolPermission::Deny
            } else {
                ToolPermission::Ask
            },
        );
    }

    pub async fn apply(&mut self, session: &SessionHandle) -> Result<()> {
        // Strategy changes do not reconfigure permissions or require stopping
        // the current run. Profile/access edits keep their existing guards.
        if fingerprint(&(&self.profile, &self.policy)) != self.saved_access {
            session
                .configure(self.profile.clone(), self.policy.clone(), &id("configure"))
                .await?;
        }
        if DelegationLevel::preference(&session.snapshot().await?.metadata)?
            != self.delegation_level
        {
            session
                .set_delegation_level(self.delegation_level, &id("delegation"))
                .await?;
        }
        if self.save_project {
            let outcome: Result<()> = async {
                let mut config = self.project.config.clone();
                config.profile = Some(self.profile.name.clone());
                config.tools = Some(self.profile.tools.clone());
                config.skills = Some(self.profile.skills.clone());
                self.project.save(&self.workspace, &config).await?;
                self.project = Project::load(&self.workspace).await?;
                save_preferences(
                    &self.workspace,
                    session.snapshot().await?.policy,
                    Some(self.delegation_level),
                )
                .await
            }
            .await;
            if let Err(error) = outcome {
                return Err(WorkerError::new(
                    "defaults_save_failed",
                    format!(
                        "Session updated, but project defaults were not fully saved: {error}. Retry Ctrl-S after resolving the error."
                    ),
                ));
            }
        }
        self.status = if self.save_project {
            "Saved: current session and project defaults. Esc returns to chat."
        } else {
            "Saved: current session. Esc returns to chat."
        }
        .into();
        if let Some(level) = self.project_delegation_level {
            self.status.push_str(&format!(
                " Effective delegation: {} (fixed by project config).",
                level.label()
            ));
        }
        self.saved_access = fingerprint(&(&self.profile, &self.policy));
        self.saved_draft = fingerprint(&(&self.profile, &self.policy, self.delegation_level));
        Ok(())
    }
}

fn toggle(items: &mut Vec<String>, name: &str) {
    if items.iter().any(|s| s == name) {
        items.retain(|s| s != name);
    } else {
        items.push(name.into());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use werk_worker::providers::{ScriptedProvider, ScriptedTurn};

    #[tokio::test]
    async fn profile_switch_keeps_saved_project_rules_for_scoped_and_legacy_sessions() {
        for scoped in [false, true] {
            let workspace = tempfile::tempdir().unwrap();
            let global = tempfile::tempdir().unwrap();
            let path = workspace.path().join("AGENTS.md");
            std::fs::write(&path, "Original immutable project requirement.").unwrap();
            let project = Project::load(workspace.path()).await.unwrap();
            let profile = if scoped {
                project
                    .scoped_profile("coding-basic", workspace.path(), global.path())
                    .await
            } else {
                project
                    .profile("coding-basic", workspace.path(), global.path())
                    .await
            }
            .unwrap();
            let prompt = profile.prompt.clone();
            let provider = Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text("done")]));
            let engine = WorkerEngine::builder(provider.clone())
                .build()
                .await
                .unwrap();
            let session = engine
                .create_session(
                    profile,
                    workspace.path(),
                    HostPolicy::default(),
                    if scoped {
                        project.scoped_metadata()
                    } else {
                        project.metadata()
                    },
                )
                .await
                .unwrap();
            let original = session.snapshot().await.unwrap();
            std::fs::write(
                &path,
                "Changed external requirement must await a new session.",
            )
            .unwrap();

            let mut editor = Editor::open(&session).await.unwrap();
            assert_eq!(editor.profile.prompt, prompt);
            assert!(!editor.is_dirty());
            editor.tab = 0;
            editor.selected[0] = editor
                .profiles
                .iter()
                .position(|p| p.name == "general-readonly")
                .unwrap();
            editor.activate();
            editor.save_project = false;
            editor.apply(&session).await.unwrap();
            let saved = session.snapshot().await.unwrap();
            assert_eq!(saved.profile.name, "general-readonly");
            assert_eq!(saved.metadata, original.metadata);
            assert_eq!(saved.profile.prompt.contains("Original immutable"), !scoped);
            session
                .submit("Inspect the saved rules.", "check", Limits::default(), None)
                .await
                .unwrap()
                .wait()
                .await
                .unwrap();
            let requests = provider.requests();
            let content = requests[0]
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(content.matches("Original immutable").count(), 1);
            assert!(!content.contains("Changed external requirement"));
            engine.close().await.unwrap();
        }
    }
}
