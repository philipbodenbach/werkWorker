//! Interactive host setup. Credentials are deliberately separate from saved settings.
use crate::cli_tui::{BG, BRAND, Restore, TEXT, banner, choice_line, clean, panel, shortcut_text};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use futures_util::StreamExt;
use ratatui::{
    layout::{Constraint, Layout},
    style::Style,
    text::Line,
    widgets::{Block, Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use werk_worker::{
    profiles::AgentProfile,
    project::{Access, Project},
    providers::{
        EnvironmentSecrets, ModelProvider, ScriptedProvider, ScriptedTurn, SecretResolver, Support,
        WerkConfig, WerkProvider,
    },
    *,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    version: u32,
    werk: WerkConfig,
    secret_env: Option<String>,
    #[serde(default)]
    no_auth: bool,
    profile: String,
    limits: Limits,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousConnection {
    settings: Settings,
    access: Access,
}

async fn previous_path(workspace: &Path) -> Result<PathBuf> {
    let workspace = tokio::fs::canonicalize(workspace).await?;
    Ok(settings_path()?
        .parent()
        .expect("config parent")
        .join("connections")
        .join(format!("{}.json", fingerprint(&workspace))))
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            werk: WerkConfig::local(""),
            secret_env: None,
            no_auth: false,
            profile: "general-readonly".into(),
            limits: Limits {
                reserve_output_tokens: 4096,
                ..Default::default()
            },
        }
    }
}
pub struct Connected {
    pub provider: Arc<dyn ModelProvider>,
    pub limits: Limits,
    pub profile: AgentProfile,
    pub access: Access,
    pub reuse_project_access: bool,
}
struct Pending(Option<tokio::task::JoinHandle<Result<WerkProvider>>>);
impl std::ops::Deref for Pending {
    type Target = Option<tokio::task::JoinHandle<Result<WerkProvider>>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for Pending {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}
struct PasteGuard;
impl Drop for PasteGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    }
}
pub(crate) fn settings_path() -> Result<PathBuf> {
    let root = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|v| PathBuf::from(v).join(".config")))
    };
    root.map(|p| p.join("werkworker/config.json"))
        .ok_or_else(|| WorkerError::new("config_path", "No user configuration directory available"))
}
pub(crate) async fn read_settings(path: &Path) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::new();
    file.take(65_537).read_to_end(&mut bytes).await?;
    if bytes.len() > 65_536 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Settings too large",
        ));
    }
    Ok(bytes)
}
pub(crate) async fn save(path: &Path, settings: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| WorkerError::new("config_path", "Invalid configuration path"))?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!("{}.tmp", id("connection")));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).await?;
    use tokio::io::AsyncWriteExt;
    file.write_all(&serde_json::to_vec_pretty(settings)?)
        .await?;
    file.sync_all().await?;
    drop(file);
    // Windows cannot atomically replace an existing file with std rename.
    #[cfg(windows)]
    if tokio::fs::try_exists(path).await? {
        tokio::fs::remove_file(path).await?;
    }
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}
struct Form {
    settings: Settings,
    stage: u8,
    field: usize,
    target: usize,
    auth: usize,
    key: String,
    env_name: String,
    context: String,
    output: String,
    models: Vec<String>,
    selected: usize,
    backend: String,
    save: usize,
    error: String,
    busy: bool,
    quit_armed: bool,
    custom_policy: bool,
    workspace: String,
    profiles: Vec<String>,
    access: Access,
    project_notice: String,
}
impl Form {
    fn new(settings: Settings, custom_policy: bool) -> Self {
        let auth = if settings.secret_env.is_some() {
            1
        } else if settings.no_auth {
            2
        } else {
            0
        };
        let target = usize::from(settings.werk.base_url.starts_with("https://"));
        let env_name = settings
            .secret_env
            .clone()
            .unwrap_or_else(|| "WERK_API_KEY".into());
        let context = settings
            .werk
            .capabilities
            .context_tokens
            .map(|n| n.to_string())
            .unwrap_or_default();
        let output = settings.limits.reserve_output_tokens.to_string();
        Self {
            settings,
            stage: 0,
            field: 0,
            target,
            auth,
            key: String::new(),
            env_name,
            context,
            output,
            models: vec![],
            selected: 0,
            backend: String::new(),
            save: 0,
            error: String::new(),
            busy: false,
            quit_armed: false,
            custom_policy,
            workspace: String::new(),
            profiles: AgentProfile::names()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            access: Access::Ask,
            project_notice: String::new(),
        }
    }
    fn fields(&self) -> Vec<(String, String)> {
        match self.stage {
            0 => vec![
                (
                    "Connection".into(),
                    [
                        "Local Werk server",
                        "Remote Werk server (HTTPS)",
                        "Offline demo (scripted)",
                    ][self.target]
                        .into(),
                ),
                ("Continue".into(), "Enter".into()),
            ],
            1 => vec![
                ("Server URL".into(), self.settings.werk.base_url.clone()),
                (
                    "Authentication".into(),
                    [
                        "Enter API key (session only)",
                        "Environment variable",
                        "No authentication",
                    ][self.auth]
                        .into(),
                ),
                (
                    if self.auth == 1 {
                        "Variable name"
                    } else {
                        "API key"
                    }
                    .into(),
                    match self.auth {
                        0 => "*".repeat(self.key.chars().count().min(32)),
                        1 => self.env_name.clone(),
                        _ => "not used".into(),
                    },
                ),
                ("Discover models".into(), "Enter to check server".into()),
            ],
            _ => vec![
                (
                    "Model".into(),
                    self.models.get(self.selected).cloned().unwrap_or_default(),
                ),
                (
                    "Context tokens".into(),
                    if self.context.is_empty() {
                        "auto (or enter per-request server limit)".into()
                    } else {
                        self.context.clone()
                    },
                ),
                (
                    "Output tokens (including reasoning)".into(),
                    self.output.clone(),
                ),
                ("Agent profile".into(), self.settings.profile.clone()),
                (
                    "Workspace access (Left/Right to change)".into(),
                    if self.custom_policy {
                        "CLI grants (unchanged)"
                    } else {
                        self.access.label()
                    }
                    .into(),
                ),
                (
                    "Tool calling".into(),
                    if self.settings.werk.capabilities.tool_calling == Support::Supported {
                        "Enabled; server/model must support tools"
                    } else {
                        "Disabled"
                    }
                    .into(),
                ),
                (
                    "Streaming".into(),
                    if self.settings.werk.stream {
                        "Enabled"
                    } else {
                        "Disabled"
                    }
                    .into(),
                ),
                (
                    "Save settings (never API key)".into(),
                    [
                        "Global connection",
                        "Project model/profile/budgets",
                        "Do not save",
                    ][self.save]
                        .into(),
                ),
                ("Connect and start".into(), "Enter".into()),
            ],
        }
    }
    fn edit(&mut self) -> Option<&mut String> {
        match (self.stage, self.field) {
            (1, 0) => Some(&mut self.settings.werk.base_url),
            (1, 2) if self.auth == 0 => Some(&mut self.key),
            (1, 2) if self.auth == 1 => Some(&mut self.env_name),
            (2, 1) => Some(&mut self.context),
            (2, 2) => Some(&mut self.output),
            _ => None,
        }
    }
    fn cycle(&mut self, forward: bool) {
        let next = |current: usize, len: usize| {
            if forward {
                (current + 1) % len
            } else {
                (current + len - 1) % len
            }
        };
        match (self.stage, self.field) {
            (0, 0) => self.target = next(self.target, 3),
            (1, 1) => {
                self.auth = next(self.auth, 3);
                self.key.clear();
            }
            (2, 0) if !self.models.is_empty() => {
                self.selected = next(self.selected, self.models.len());
                self.context.clear();
            }
            (2, 3) => {
                let names = &self.profiles;
                let i = names
                    .iter()
                    .position(|n| *n == self.settings.profile)
                    .unwrap_or(0);
                self.settings.profile = names[next(i, names.len())].clone();
            }
            (2, 4) if !self.custom_policy => {
                let i = Access::ALL
                    .iter()
                    .position(|v| *v == self.access)
                    .unwrap_or(1);
                self.access = Access::ALL[next(i, Access::ALL.len())];
            }
            (2, 5) => {
                self.settings.werk.capabilities.tool_calling =
                    if self.settings.werk.capabilities.tool_calling == Support::Supported {
                        Support::Unsupported
                    } else {
                        Support::Supported
                    }
            }
            (2, 6) => self.settings.werk.stream = !self.settings.werk.stream,
            (2, 7) => self.save = next(self.save, 3),
            _ => {}
        }
    }
    fn secret(&self) -> Result<Option<String>> {
        match self.auth {
            0 if self.key.trim().is_empty() => Err(WorkerError::new(
                "missing_key",
                "Enter the server API key or select another authentication method",
            )),
            0 => Ok(Some(self.key.clone())),
            1 => EnvironmentSecrets
                .resolve(&self.env_name)?
                .filter(|v| !v.trim().is_empty())
                .map(Some)
                .ok_or_else(|| {
                    WorkerError::new(
                        "missing_key",
                        "The named environment variable is absent or empty",
                    )
                }),
            _ => Ok(None),
        }
    }
}
fn draw(frame: &mut ratatui::Frame, form: &Form) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(BG).fg(TEXT)),
        area,
    );
    let large = area.width >= 94 && area.height >= 30;
    let rows = Layout::vertical([
        Constraint::Length(if large { 8 } else { 2 }),
        Constraint::Min(3),
        Constraint::Length(5),
    ])
    .split(area);
    if large {
        frame.render_widget(Paragraph::new(banner()), rows[0]);
    } else {
        frame.render_widget(
            Paragraph::new(" werkWorker · Connection setup").style(Style::default().fg(BRAND[0])),
            rows[0],
        );
    }
    let fields = form.fields();
    let mut lines = vec![
        Line::styled(
            match form.stage {
                0 => "Choose how to connect",
                1 => "Server and credentials",
                _ => "Model and agent settings",
            },
            Style::default().fg(BRAND[0]).bold(),
        ),
        Line::from(""),
    ];
    for (i, (label, value)) in fields.iter().enumerate() {
        lines.push(choice_line(
            format!(
                "{}{label}: {}",
                if i == form.field { " › " } else { "   " },
                clean(value)
            ),
            i == form.field,
            rows[1].width.saturating_sub(2),
        ));
        lines.push(Line::from(""));
    }
    if form.stage == 2 {
        if !form.project_notice.is_empty() {
            lines.push(Line::from(clean(&form.project_notice)));
        }
        lines.push(Line::from(format!(
            "Backend reported by Werk: {}",
            clean(&form.backend)
        )));
    }
    lines.push(Line::from(format!("Workspace: {}", clean(&form.workspace))));
    lines.push(Line::from(
        "Inference backends (CUDA/CPU/etc.) are managed by the Werk server.",
    ));
    let scroll = if rows[1].height < 25 {
        ((form.field * 2).saturating_sub(rows[1].height.saturating_sub(8) as usize)) as u16
    } else {
        0
    };
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false })
            .block(panel(" Setup ", BRAND[3])),
        rows[1],
    );
    let footer = if form.busy {
        "Checking server... Esc cancels the check. Ctrl-Q exits.".to_owned()
    } else if !form.error.is_empty() {
        form.error.clone()
    } else {
        if form.stage == 2 && matches!(form.field, 3 | 4) {
            "All tools: file changes, sandbox processes and network requests require explicit approval.\nOther access modes restrict the profile's tools to the selected grants.\nLeft/Right: change selection · Tab: next field · Enter: next / action\nExisting sessions: /permissions changes access without reconnecting.".into()
        } else {
            "Tab / Shift-Tab or Up / Down: field · Left / Right: select\nEnter: next / action · Ctrl-U: clear field · Esc: back\nCtrl-Q: exit · Ctrl-C twice: exit · API keys are masked.".into()
        }
    };
    frame.render_widget(
        Paragraph::new(if form.error.is_empty() {
            shortcut_text(&footer)
        } else {
            ratatui::text::Text::styled(clean(&footer), Style::default().fg(BRAND[4]).bold())
        })
        .wrap(Wrap { trim: false }),
        rows[2],
    );
}
fn error_message(error: &WorkerError) -> String {
    match error.code.as_str() {
        "auth_error" => "Authentication failed. Check the key accepted by this server.".into(),
        "invalid_service" | "protocol_version" | "protocol_mismatch" => {
            "This endpoint is not a supported Werk server.".into()
        }
        "transport_denied" => {
            "Use a bare HTTPS server URL, or HTTP on a literal loopback address.".into()
        }
        "missing_key" => error.message.clone(),
        _ => format!(
            "Connection failed ({}). Check the URL, server and model settings.",
            error.code
        ),
    }
}
async fn resolve_settings(
    mut settings: Settings,
    seed: Option<WerkConfig>,
    secret_env: Option<String>,
    workspace: &Path,
    project: &Project,
    explicit_profile: Option<&str>,
) -> Result<(Settings, Option<String>)> {
    let mut api_key = None;
    // An explicit transport bypasses local discovery, including invalid local files.
    if seed.is_none()
        && let Some(connection) = Project::connection_config(workspace).await?
    {
        if settings.werk.base_url != connection.werk.base_url || connection.api_key.is_some() {
            settings.secret_env = None;
            settings.no_auth = false;
        }
        settings.werk = connection.werk;
        api_key = connection.api_key;
    }
    if let Some(model) = &project.config.model {
        settings.werk.model = model.clone();
    }
    if let Some(profile) = &project.config.profile {
        settings.profile = profile.clone();
    }
    if let Some(context) = project.config.context_tokens {
        settings.werk.capabilities.context_tokens = Some(context);
    }
    if let Some(output) = project.config.output_tokens {
        settings.limits.reserve_output_tokens = output;
    }
    if let Some(profile) = explicit_profile {
        settings.profile = profile.into();
    }
    if let Some(config) = seed {
        settings.werk = config;
    }
    if let Some(name) = secret_env {
        settings.secret_env = Some(name);
        api_key = None;
    }
    Ok((settings, api_key))
}

pub async fn show(
    seed: Option<WerkConfig>,
    secret_env: Option<String>,
    custom_policy: bool,
    workspace: &Path,
    project: &Project,
    explicit_profile: Option<&str>,
    force_setup: bool,
) -> Result<Option<Connected>> {
    let path = settings_path()?;
    let (settings, load_error) = match read_settings(&path).await {
        Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
            Ok(s) if s.version == 1 => (s, String::new()),
            _ => (
                Settings::default(),
                "Saved connection is invalid. Review settings before saving a replacement.".into(),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Settings::default(), String::new()),
        Err(_) => (
            Settings::default(),
            "Saved connection could not be read. Review settings.".into(),
        ),
    };
    let previous_path = previous_path(workspace).await?;
    let previous = if !force_setup && seed.is_none() && secret_env.is_none() {
        match read_settings(&previous_path).await {
            Ok(bytes) => serde_json::from_slice::<PreviousConnection>(&bytes)
                .ok()
                .filter(|p| p.settings.version == 1),
            Err(_) => None,
        }
    } else {
        None
    };
    let can_continue = !force_setup
        && seed.is_none()
        && secret_env.is_none()
        && (previous.is_some() || (!settings.werk.model.is_empty() && load_error.is_empty()));
    let previous_access = previous.as_ref().map_or(Access::Read, |p| p.access);
    let (settings, api_key) = resolve_settings(
        settings,
        seed,
        secret_env,
        workspace,
        project,
        explicit_profile,
    )
    .await?;
    let mut form = Form::new(settings, custom_policy);
    form.key = api_key.unwrap_or_default();
    form.error = load_error;
    form.workspace = workspace.display().to_string();
    if project.config_hash.is_some() {
        form.save = 1;
    }
    let global = path.parent().expect("config parent");
    form.profiles = Project::profile_names(workspace, global).await?;
    form.project_notice = format!(
        "Project: {} rule files; access requests never grant rights{}",
        project.instructions.len(),
        project
            .config
            .requested_access
            .map(|a| format!("; requested: {}", a.label()))
            .unwrap_or_default()
    );
    if can_continue {
        // Continue uses the effective transport that actually connected, including
        // verified capability choices. Project defaults may still be incomplete
        // or point at a different service selected before the last setup.
        let settings = previous.map_or_else(|| form.settings.clone(), |p| p.settings);
        let key = if settings.werk.base_url == form.settings.werk.base_url {
            form.key.clone()
        } else {
            String::new()
        };
        let mut form = Form::new(settings, custom_policy);
        form.key = key;
        if let Some(profile) = explicit_profile.or(project.config.profile.as_deref()) {
            form.settings.profile = profile.into();
        }
        let saved_access = crate::cli_config::load_policy(workspace).await?;
        let access_label = saved_access
            .as_ref()
            .map(crate::cli_config::policy_label)
            .unwrap_or_else(|| previous_access.label().into());
        let mut error = String::new();
        loop {
            let detail = format!(
                "{}\nModel: {} · Profile: {} · Access: {}\n{}",
                form.settings.werk.base_url,
                form.settings.werk.model,
                form.settings.profile,
                if custom_policy {
                    "explicit grants file"
                } else {
                    &access_label
                },
                error
            );
            match crate::cli_start::choose(
                "Connection",
                &detail,
                &[
                    "Continue previous\n  Reconnect to the saved server / service".into(),
                    "Connect new\n  Open connection setup".into(),
                ],
            )
            .await?
            {
                None => return Ok(None),
                Some(1) => break,
                _ => {}
            }
            let secret = match form.secret() {
                Ok(secret) => secret,
                Err(e) => {
                    error = format!(
                        "{} Choose Connect new to enter credentials.",
                        error_message(&e)
                    );
                    continue;
                }
            };
            let outcome = crate::cli_start::connecting(
                &form.settings.werk.base_url,
                WerkProvider::connect(form.settings.werk.clone(), secret),
            )
            .await;
            match outcome {
                Ok(None) => continue,
                Err(e) => {
                    error = error_message(&e);
                    continue;
                }
                Ok(Some(provider)) => {
                    let Some(context) = provider.capabilities().context_tokens else {
                        error = "Context limit unknown. Choose Connect new to set it.".into();
                        continue;
                    };
                    form.settings.limits.context_tokens = context.min(2_000_000);
                    if form.settings.limits.validate().is_err()
                        || form.settings.limits.reserve_output_tokens > context / 4
                    {
                        error = "Saved output budget exceeds the current server capacity. Choose Connect new.".into();
                        continue;
                    }
                    let profile = match project
                        .profile(&form.settings.profile, workspace, global)
                        .await
                    {
                        Ok(profile) => profile,
                        Err(e) => {
                            error = format!(
                                "Saved profile cannot be loaded ({}). Choose Connect new.",
                                e.code
                            );
                            continue;
                        }
                    };
                    save(
                        &previous_path,
                        &PreviousConnection {
                            settings: form.settings.clone(),
                            access: previous_access,
                        },
                    )
                    .await?;
                    return Ok(Some(Connected {
                        provider: Arc::new(provider),
                        limits: form.settings.limits,
                        profile,
                        access: previous_access,
                        reuse_project_access: true,
                    }));
                }
            }
        }
    }
    let mut terminal = ratatui::try_init()?;
    let _restore = Restore;
    crossterm::execute!(std::io::stdout(), EnableBracketedPaste)?;
    let _paste = PasteGuard;
    let mut keys = crate::cli_tui::input().await;
    let mut pending = Pending(None);
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut result = None;
    loop {
        terminal.draw(|f| draw(f, &form))?;
        tokio::select! {
            _=tick.tick(), if pending.is_some()=>{
                if pending.as_ref().is_some_and(|task|task.is_finished()) {
                    let outcome=pending.take().unwrap().await.map_err(|_|WorkerError::new("connection_task","Connection check stopped"))?;
                    form.busy=false;
                    match outcome {
                        Err(error)=>form.error=error_message(&error),
                        Ok(provider) if form.stage==1=>{
                            form.models=provider.discovery["models"]["data"].as_array().into_iter().flatten().filter_map(|v|v["id"].as_str()).filter(|s|!s.is_empty() && s.len()<=512 && !s.chars().any(char::is_control)).take(256).map(str::to_owned).collect();
                            if form.models.is_empty(){form.error="The server returned no usable models. Load/import a model in Werk first.".into();continue;}
                            form.selected=form.models.iter().position(|m|*m==form.settings.werk.model).unwrap_or(0);
                            form.backend=provider.discovery["info"]["active_backend"].as_str().unwrap_or("managed by server").to_owned();
                            form.settings.werk.capabilities.tool_calling=Support::Supported;
                            form.stage=2;form.field=0;form.error.clear();
                        },
                        Ok(provider)=>{
                            let Some(context)=provider.capabilities().context_tokens else {form.error="Context limit unknown. Enter the server's per-request context tokens.".into();form.field=1;continue;};
                            form.settings.limits.context_tokens=context.min(2_000_000);
                            if form.settings.limits.validate().is_err() || form.settings.limits.reserve_output_tokens>context/4 {
                                form.error=format!("Output tokens must be 1..{} for this server context ({context}).",context/4);form.field=2;continue;
                            }
                            let saved=match form.save {
                                0=>save(&path,&form.settings).await,
                                1=>{
                                    let mut config=project.config.clone();
                                    config.model=Some(form.settings.werk.model.clone());config.profile=Some(form.settings.profile.clone());
                                    config.context_tokens=form.settings.werk.capabilities.context_tokens;config.output_tokens=Some(form.settings.limits.reserve_output_tokens);
                                    project.save(workspace,&config).await
                                },
                                _=>Ok(())
                            };
                            if let Err(error)=saved {form.error=format!("Connection works, but saving failed ({}). Choose Do not save to continue.",error.code);form.field=7;continue;}
                            if form.save < 2
                                && let Err(error)=save(&previous_path,&PreviousConnection{settings:form.settings.clone(),access:form.access}).await {
                                    form.error=format!("Cannot remember connection ({}). Choose Do not save to continue.",error.code);form.field=7;continue;
                            }
                            if form.save < 2 && !custom_policy
                                && let Err(error) = crate::cli_config::save_policy(workspace, form.access.policy()).await {
                                form.error=format!("Cannot save workspace access: {error}");continue;
                            }
                            result=Some(Connected{provider:Arc::new(provider),limits:form.settings.limits.clone(),profile:project.profile(&form.settings.profile,workspace,global).await?,access:form.access,reuse_project_access:false});break;
                        }
                    }
                }
            },
            event=keys.next()=>{
                let Some(event)=event else {break;};
                match event? {
                    Event::Paste(value) if !form.busy=>{if let Some(field)=form.edit(){for c in value.chars().filter(|c|!c.is_control()).take(4096){if field.len()+c.len_utf8()<=4096{field.push(c);}}}},
                    Event::Key(key) if key.kind!=KeyEventKind::Release=>{
                        let ctrl=key.modifiers.contains(KeyModifiers::CONTROL);
                        if ctrl && key.code==KeyCode::Char('q'){break;}
                        if ctrl && key.code==KeyCode::Char('c') {
                            if key.kind==KeyEventKind::Repeat{continue;}
                            if form.quit_armed {break;} form.quit_armed=true;form.error="Press Ctrl-C again to exit setup.".into();
                            if let Some(task)=pending.take(){task.abort();let _=task.await;form.busy=false;}
                            continue;
                        }
                        form.quit_armed=false;
                        if key.code==KeyCode::Esc {
                            if let Some(task)=pending.take(){task.abort();let _=task.await;form.busy=false;}
                            else {form.stage=form.stage.saturating_sub(1);form.field=0;}
                            form.error.clear();continue;
                        }
                        if form.busy{continue;}
                        if ctrl && key.code==KeyCode::Char('u'){if let Some(field)=form.edit(){field.clear();}continue;}
                        match key.code {
                            KeyCode::Tab|KeyCode::Down=>form.field=(form.field+1)%form.fields().len(),
                            KeyCode::BackTab|KeyCode::Up=>form.field=(form.field+form.fields().len()-1)%form.fields().len(),
                            KeyCode::Left=>form.cycle(false),KeyCode::Right=>form.cycle(true),
                            KeyCode::Backspace=>{if let Some(field)=form.edit(){field.pop();}},
                            KeyCode::Char(c) if !ctrl && !c.is_control()=>{if let Some(field)=form.edit(){if field.len()+c.len_utf8()<=4096{field.push(c);}}else if c==' ' {form.cycle(true);}},
                            KeyCode::Enter=>{
                                if form.stage==0 {
                                    if form.target==2 {result=Some(Connected{provider:Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text("werkWorker offline demo. No server or model is connected.")])),limits:Limits::default(),profile:AgentProfile::bundled("general-readonly")?,access:Access::None,reuse_project_access:false});break;}
                                    if form.target==1 && form.settings.werk.base_url.starts_with("http://127."){form.settings.werk.base_url="https://".into();}
                                    form.stage=1;form.field=0;form.error.clear();continue;
                                }
                                if form.field+1!=form.fields().len(){form.field+=1;continue;}
                                let secret=match form.secret(){Ok(secret)=>secret,Err(error)=>{form.error=error_message(&error);form.field=2;continue;}};
                                let mut config=form.settings.werk.clone();
                                if form.stage==2 {
                                    let profile=match project.profile(&form.settings.profile,workspace,global).await {Ok(profile)=>profile,Err(error)=>{form.error=format!("Profile/project settings invalid ({}). Check the configured tools, skills and profile.",error.code);form.field=3;continue;}};
                                    config.model=form.models[form.selected].clone();
                                    config.capabilities.context_tokens=if form.context.trim().is_empty(){None}else{match form.context.parse::<usize>(){Ok(n) if (1024..=2_000_000).contains(&n)=>Some(n),_=>{form.error="Context tokens must be 1024..2000000, or empty for discovery.".into();form.field=1;continue;}}};
                                    let output=match form.output.parse::<usize>(){Ok(n) if (1..=500_000).contains(&n)=>n,_=>{form.error="Enter a positive output token budget.".into();form.field=2;continue;}};
                                    form.settings.limits.reserve_output_tokens=output;
                                    config.capabilities.streaming=if config.stream{Support::Supported}else{Support::Unsupported};
                                    if !profile.tools.is_empty() && config.capabilities.tool_calling!=Support::Supported {form.error="This profile requires tool calling. Enable it for a compatible model.".into();form.field=5;continue;}
                                    form.settings.werk=config.clone();
                                    form.settings.secret_env=if form.auth==1{Some(form.env_name.clone())}else{None};
                                    form.settings.no_auth=form.auth==2;
                                }
                                form.error.clear();form.busy=true;
                                pending.0=Some(tokio::spawn(async move {tokio::time::timeout(Duration::from_secs(20),WerkProvider::connect(config,secret)).await.map_err(|_|WorkerError::new("connection_timeout","Connection timed out"))?}));
                            },_=>{}
                        }
                    },_=>{}
                }
            }
        }
    }
    if let Some(task) = pending.take() {
        task.abort();
        let _ = task.await;
    }
    form.key.clear();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn workspace_connection_credentials_and_explicit_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = dir.path().join(".werkworker");
        std::fs::create_dir(&metadata).unwrap();
        let mut local = WerkConfig::local("local-model");
        local.base_url = "http://127.0.0.1:12345".into();
        std::fs::write(
            metadata.join("werk-config.json"),
            serde_json::to_vec(&local).unwrap(),
        )
        .unwrap();
        std::fs::write(
            metadata.join("secrets.json"),
            r#"{"api_key":"local-fixture-key"}"#,
        )
        .unwrap();
        std::fs::write(
            metadata.join("config.json"),
            r#"{"model":"project-model","output_tokens":3072}"#,
        )
        .unwrap();
        let project = Project::load(dir.path()).await.unwrap();
        let global = Settings {
            secret_env: Some("GLOBAL_KEY".into()),
            ..Default::default()
        };
        let (settings, key) =
            resolve_settings(global.clone(), None, None, dir.path(), &project, None)
                .await
                .unwrap();
        assert_eq!(settings.werk.base_url, local.base_url);
        assert_eq!(settings.werk.model, "project-model");
        assert_eq!(settings.limits.reserve_output_tokens, 3072);
        assert!(settings.secret_env.is_none());
        assert_eq!(key.as_deref(), Some("local-fixture-key"));
        assert!(
            !serde_json::to_string(&settings)
                .unwrap()
                .contains("local-fixture-key")
        );
        assert!(!project.metadata().to_string().contains("local-fixture-key"));

        let (settings, key) = resolve_settings(
            global.clone(),
            None,
            Some("EXPLICIT_KEY".into()),
            dir.path(),
            &project,
            None,
        )
        .await
        .unwrap();
        assert_eq!(settings.secret_env.as_deref(), Some("EXPLICIT_KEY"));
        assert!(key.is_none());

        // A different local endpoint must not inherit the global credential.
        std::fs::remove_file(metadata.join("secrets.json")).unwrap();
        let (settings, _) =
            resolve_settings(global.clone(), None, None, dir.path(), &project, None)
                .await
                .unwrap();
        assert!(settings.secret_env.is_none());

        // Explicit transport overrides also bypass malformed project connection files.
        std::fs::write(metadata.join("werk-config.json"), "invalid json").unwrap();
        assert!(
            resolve_settings(global.clone(), None, None, dir.path(), &project, None)
                .await
                .is_err()
        );
        let (settings, key) = resolve_settings(
            global,
            Some(WerkConfig::local("explicit-model")),
            None,
            dir.path(),
            &project,
            Some("coding-basic"),
        )
        .await
        .unwrap();
        assert_eq!(settings.werk.model, "explicit-model");
        assert_eq!(settings.profile, "coding-basic");
        assert!(key.is_none());
    }

    #[test]
    fn credentials_are_masked_and_never_serialized() {
        let mut form = Form::new(Settings::default(), false);
        form.stage = 1;
        form.key = "sensitive-fixture-key".into();
        assert!(!format!("{:?}", form.fields()).contains(&form.key));
        assert!(
            !serde_json::to_string(&form.settings)
                .unwrap()
                .contains(&form.key)
        );
        for size in [(110, 36), (30, 10), (1, 1)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(size.0, size.1)).unwrap();
            terminal.draw(|f| draw(f, &form)).unwrap();
            let screen = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(!screen.contains(&form.key));
        }
    }
}
