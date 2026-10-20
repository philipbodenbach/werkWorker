//! Interactive session configuration and explicitly saved workspace defaults.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use werk_worker::{
    policy::HostPolicy,
    profiles::AgentProfile,
    project::{Access, Project},
    *,
};

#[derive(Serialize, Deserialize)]
struct SavedPolicy {
    workspace_identity: String,
    policy: HostPolicy,
}

async fn policy_path(workspace: &Path) -> Result<PathBuf> {
    let root = tokio::fs::canonicalize(workspace).await?;
    Ok(crate::cli_setup::settings_path()?
        .parent()
        .expect("config parent")
        .join("workspace-access")
        .join(format!("{}.json", fingerprint(&root))))
}

pub async fn load_policy(workspace: &Path) -> Result<Option<HostPolicy>> {
    let bytes = match crate::cli_setup::read_settings(&policy_path(workspace).await?).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let saved: SavedPolicy = serde_json::from_slice(&bytes)?;
    let current = runtime::Workspace::open(workspace).await?;
    Ok((saved.workspace_identity == current.identity).then_some(saved.policy))
}

pub async fn save_policy(workspace: &Path, policy: HostPolicy) -> Result<()> {
    let current = runtime::Workspace::open(workspace).await?;
    crate::cli_setup::save(
        &policy_path(workspace).await?,
        &SavedPolicy {
            workspace_identity: current.identity,
            policy,
        },
    )
    .await
}

pub fn policy_label(policy: &HostPolicy) -> String {
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

pub const TABS: [&str; 4] = ["Profiles", "Permissions", "Tools", "Skills"];

pub struct Editor {
    pub profile: AgentProfile,
    pub policy: HostPolicy,
    pub tab: usize,
    pub selected: [usize; 4],
    pub save_project: bool,
    pub status: String,
    profiles: Vec<AgentProfile>,
    tools: Vec<ToolDefinition>,
    skills: Vec<skills::Skill>,
    project: Project,
    workspace: PathBuf,
}

impl Editor {
    pub async fn open(session: &SessionHandle) -> Result<Self> {
        let data = session.snapshot().await?;
        let project = Project::load(&data.workspace).await?;
        let global = crate::cli_setup::settings_path()?
            .parent()
            .expect("config parent")
            .to_path_buf();
        // Selecting a preset restores its own tool/skill set; project overrides
        // remain the initial draft until a preset is explicitly selected.
        let mut presets = project.clone();
        presets.config.tools = None;
        presets.config.skills = None;
        let mut profiles = Vec::new();
        for name in Project::profile_names(&data.workspace, &global).await? {
            profiles.push(presets.profile(&name, &data.workspace, &global).await?);
        }
        if !profiles.iter().any(|p| p.name == data.profile.name) {
            profiles.push(data.profile.clone());
        }
        let selected = profiles
            .iter()
            .position(|p| p.name == data.profile.name)
            .unwrap_or(0);
        Ok(Self {
            profile: data.profile,
            policy: data.policy,
            tab: 0,
            selected: [selected, 0, 0, 0],
            save_project: true,
            status: String::new(),
            profiles,
            tools: session.available_tools(),
            skills: session.available_skills(),
            project,
            workspace: data.workspace,
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
            1 => std::iter::once(format!("Keep current: {}", policy_label(&self.policy)))
                .chain(Access::ALL.iter().map(|a| a.label().to_string()))
                .collect(),
            2 => self
                .tools
                .iter()
                .map(|t| {
                    format!(
                        "[{}] {}{}",
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
                        }
                    )
                })
                .collect(),
            _ => self
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
                }
            }
            1 => {
                if let Some(access) = selected.checked_sub(1).and_then(|i| Access::ALL.get(i)) {
                    let denied = self.policy.hard_denied_tools.clone();
                    self.policy = access.policy();
                    self.policy.hard_denied_tools = denied;
                }
            }
            2 => {
                if let Some(tool) = self.tools.get(selected) {
                    toggle(&mut self.profile.tools, &tool.name);
                }
            }
            _ => {
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
        }
        self.status =
            "Draft changed. Ctrl-S applies settings; Esc discards unsaved changes.".into();
    }

    pub fn details(&self) -> String {
        let selected = self.selected[self.tab];
        let detail = match self.tab {
            0 => self.profiles.get(selected).map(|p| format!("{}\n\nTools: {}\nSkills: {}\n\n{}", p.name, p.tools.join(", "), p.skills.join(", "), p.prompt)),
            1 => Some("Select an access level with Enter/Space. This replaces scoped grants with the selected preset. Hard denies stay in force. Tools and skills are selected separately; enabling a tool does not itself grant access.".into()),
            2 => self.tools.get(selected).map(|t| format!("{}\nEffect: {:?}\n\n{}", t.name, t.effect, t.description)),
            _ => self.skills.get(selected).map(|s| format!("{}\n{}\n\n{}", s.name, s.description, s.body)),
        }.unwrap_or_default();
        format!(
            "Draft profile: {}\n{}\nEnabled: {} tools, {} skills\nSave: {}\n\n{}\n\n{}",
            self.profile.name,
            policy_label(&self.policy),
            self.profile.tools.len(),
            self.profile.skills.len(),
            if self.save_project {
                "current session + project defaults"
            } else {
                "current session only"
            },
            detail,
            self.status
        )
    }

    pub async fn apply(&mut self, session: &SessionHandle) -> Result<()> {
        session
            .configure(self.profile.clone(), self.policy.clone(), &id("configure"))
            .await?;
        if self.save_project {
            let outcome: Result<()> = async {
                let mut config = self.project.config.clone();
                config.profile = Some(self.profile.name.clone());
                config.tools = Some(self.profile.tools.clone());
                config.skills = Some(self.profile.skills.clone());
                self.project.save(&self.workspace, &config).await?;
                self.project = Project::load(&self.workspace).await?;
                save_policy(&self.workspace, session.snapshot().await?.policy).await
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
