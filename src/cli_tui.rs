//! Interactive CLI client. Engine policy and lifecycle stay in the public SDK.
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
#[path = "cli_tui_input.rs"]
mod text_input;
use futures_util::{Stream, StreamExt};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
};
use std::{io::IsTerminal, pin::Pin, time::Duration};
use werk_worker::*;

// Exact werk1112 top/terminal palette; attribution in NOTICE.
pub(crate) const BG: Color = Color::Rgb(9, 13, 23);
const PANEL: Color = Color::Rgb(15, 21, 35);
pub(crate) const TEXT: Color = Color::Rgb(224, 230, 248);
pub(crate) const MUTED: Color = Color::Rgb(129, 143, 174);
const ACTION_BG: Color = Color::Rgb(25, 39, 62);
const HEADING: Color = Color::Rgb(192, 168, 255);
const ENABLED: Color = Color::Rgb(120, 225, 170);
const RESTRICTED: Color = Color::Rgb(255, 205, 115);
pub(crate) const BRAND: [Color; 5] = [
    Color::Rgb(0, 220, 255),
    Color::Rgb(59, 130, 246),
    Color::Rgb(99, 102, 241),
    Color::Rgb(139, 92, 246),
    Color::Rgb(255, 79, 195),
];

pub(crate) fn choice_style(selected: bool) -> Style {
    if selected {
        Style::default().fg(BG).bg(BRAND[0]).bold()
    } else {
        Style::default().fg(BRAND[0]).bg(ACTION_BG).bold()
    }
}

pub(crate) fn choice_line(text: String, selected: bool, width: u16) -> Line<'static> {
    let mut line = if !selected && let Some((prefix, suffix)) = text.split_once("[x]") {
        Line::from(vec![
            Span::raw(prefix.to_owned()),
            Span::styled("[x]", Style::default().fg(ENABLED)),
            Span::raw(suffix.to_owned()),
        ])
        .style(choice_style(false))
    } else {
        Line::styled(text, choice_style(selected))
    };
    let padding = usize::from(width).saturating_sub(line.width());
    line.spans.push(Span::raw(" ".repeat(padding)));
    line
}

pub(crate) fn shortcut_text(text: &str) -> Text<'static> {
    Text::from(
        clean(text)
            .lines()
            .map(|line| {
                Line::from(
                    line.split_inclusive(' ')
                        .map(|word| {
                            let key = word.trim().trim_end_matches([':', ',', '.']);
                            let is_key = key.starts_with("Ctrl-")
                                || key.starts_with("Shift-")
                                || key.starts_with("Alt-")
                                || key.starts_with("F2")
                                || key.starts_with("F3")
                                || key.starts_with("F4")
                                || key.starts_with("F5")
                                || key.starts_with("F6")
                                || key.starts_with("Tab")
                                || key.starts_with("Enter")
                                || key.starts_with("Esc")
                                || key.starts_with("Space")
                                || key.starts_with("PgUp")
                                || key.starts_with("PgDn")
                                || key.starts_with("↑")
                                || key.starts_with("←")
                                || matches!(
                                    key,
                                    "Up" | "Down"
                                        | "Up/Down"
                                        | "Left"
                                        | "Right"
                                        | "Home"
                                        | "End"
                                        | "P"
                                        | "/help"
                                );
                            Span::styled(
                                word.to_owned(),
                                if is_key {
                                    Style::default().fg(BRAND[0]).bg(ACTION_BG).bold()
                                } else {
                                    Style::default().fg(TEXT)
                                },
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>(),
    )
}

fn information_text(text: &str) -> Text<'static> {
    let mut section = "";
    Text::from(
        clean(text)
            .lines()
            .map(|line| {
                if line.starts_with("E / F5:") || line.starts_with("Enter: review") {
                    let (key, description) = line.split_once(':').expect("action separator");
                    return Line::from(vec![
                        Span::styled(format!("{key}:"), choice_style(true)),
                        Span::styled(
                            description.to_string(),
                            Style::default().fg(TEXT).bg(ACTION_BG).bold(),
                        ),
                    ])
                    .style(Style::default().bg(ACTION_BG));
                }
                if line.starts_with("F2 permissions") || line.starts_with("F4 view") {
                    return shortcut_text(line).lines.remove(0);
                }
                if let Some((label, value)) = line.split_once(':')
                    && matches!(
                        label,
                        "Profile"
                            | "Workspace writes"
                            | "Process tool"
                            | "Workspace"
                            | "Available profile tools"
                            | "Host grants"
                            | "Hard-denied tools"
                            | "Available skills"
                            | "Status"
                            | "Model requests"
                            | "Tool calls"
                            | "Draft profile"
                            | "Enabled"
                            | "Save"
                            | "Tools"
                            | "Skills"
                            | "Effect"
                    )
                {
                    section = match label {
                        "Available profile tools" => "tools",
                        "Host grants" => "grants",
                        _ => "",
                    };
                    let color = if value.contains("disabled")
                        || value.contains("not ")
                        || value.contains("approval")
                        || (label == "Hard-denied tools" && value.trim() != "None")
                    {
                        RESTRICTED
                    } else if matches!(
                        label,
                        "Workspace writes" | "Process tool" | "Available skills" | "Enabled"
                    ) {
                        ENABLED
                    } else {
                        TEXT
                    };
                    return Line::from(vec![
                        Span::styled(format!("{label}:"), Style::default().fg(HEADING).bold()),
                        Span::styled(value.to_owned(), Style::default().fg(color)),
                    ]);
                }
                if line.is_empty() {
                    section = "";
                }
                Line::styled(
                    line.to_owned(),
                    Style::default().fg(match section {
                        "tools" => BRAND[0],
                        "grants" => RESTRICTED,
                        _ => TEXT,
                    }),
                )
            })
            .collect::<Vec<_>>(),
    )
}

// Same five-row slanted letterforms as werk1112's startup banner.
// Fixed glyph advances keep the top strokes aligned; the shared row indent
// supplies the slant without accumulating offsets between letters.
const W: [&str; 5] = [
    r"__        __",
    r"\ \      / /",
    r"  \ \ /\ / /",
    r"    \ V  V /",
    r"      \_/\_/",
];
const E: [&str; 5] = [r" ______", r"/ ____/", r"/ __/  ", r"/ /___ ", r"/_____/"];
const R: [&str; 5] = [r" ____  ", r"/ __ \", r"/ /_/ /", r"/ _, _/", r"/_/ |_|"];
const K: [&str; 5] = [r" __ __ ", r"/ //_/", r"/ ,<  ", r"/ /| | ", r"/_/ |_|"];
const O: [&str; 5] = [r" ____  ", r"/ __ \ ", r"/ / / /", r"/ /_/ /", r"\____/ "];
const COMMANDS: [(&str, &str); 10] = [
    ("/help", "Show keyboard shortcuts and commands"),
    ("/clear", "Clear the visible conversation"),
    ("/cancel", "Cancel the active run"),
    ("/quit", "Close werkWorker"),
    ("/agents", "Browse subagent progress and tool results"),
    (
        "/permissions",
        "Show active tools, rights and how to change them",
    ),
    ("/setup", "Configure server, model and credentials"),
    ("/view", "Choose Compact or Cockpit layout"),
    ("/sessions", "Start a new chat or open a previous session"),
    ("/settings", "Edit profile, permissions, tools and skills"),
];
const RESUME_REQUIRED: &str =
    "Resume the interrupted run via /sessions before sending another message.";
pub(crate) fn banner() -> Vec<Line<'static>> {
    let glyphs = [W, E, R, K, W, O, R, K, E, R];
    let mut lines = Vec::new();
    for row in 0..5 {
        lines.push(Line::from(
            std::iter::once(Span::raw(" ".repeat(4 - row.max(1))))
                .chain(glyphs.iter().enumerate().map(|(i, glyph)| {
                    let width = if i == 0 || i == 4 { 14 } else { 7 };
                    Span::styled(
                        format!("{:<width$}", glyph[row]),
                        Style::default().fg(BRAND[i.min(4)]).bold(),
                    )
                }))
                .collect::<Vec<_>>(),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        format!(
            "   werkWorker · Agent Engine · v{}",
            env!("CARGO_PKG_VERSION")
        ),
        Style::default().fg(BRAND[0]),
    ));
    lines
}

pub(crate) struct Restore;
/// Crossterm's background reader is process-global and its stream Drop wakes
/// but does not join the polling thread. Recreating streams during a screen
/// transition can leave the new screen waiting on the old thread's wakeup.
/// Keep one stream for the interactive process; screens borrow it exclusively.
pub(crate) async fn input() -> tokio::sync::MutexGuard<'static, EventStream> {
    static INPUT: std::sync::OnceLock<tokio::sync::Mutex<EventStream>> = std::sync::OnceLock::new();
    INPUT
        .get_or_init(|| tokio::sync::Mutex::new(EventStream::new()))
        .lock()
        .await
}
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste
        );
        ratatui::restore();
    }
}
pub(crate) fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}
#[derive(Default)]
enum Screen {
    #[default]
    Conversation,
    Agents,
    Permissions,
    PermissionConsent,
    Appearance,
    Settings,
}
#[derive(Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum LayoutMode {
    #[default]
    Compact,
    Cockpit,
}
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct UiPreferences {
    #[serde(default)]
    layout: LayoutMode,
}
fn preferences_path() -> Result<std::path::PathBuf> {
    Ok(crate::cli_setup::settings_path()?.with_file_name("ui.json"))
}
async fn load_layout() -> Result<LayoutMode> {
    match tokio::fs::read(preferences_path()?).await {
        Ok(bytes) => Ok(serde_json::from_slice::<UiPreferences>(&bytes)?.layout),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LayoutMode::Compact),
        Err(error) => Err(error.into()),
    }
}
async fn save_layout(layout: LayoutMode) -> Result<()> {
    let path = preferences_path()?;
    let parent = path.parent().expect("configuration parent");
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!("{}.tmp", id("ui")));
    tokio::fs::write(
        &temporary,
        serde_json::to_vec_pretty(&UiPreferences { layout })?,
    )
    .await?;
    #[cfg(windows)]
    if tokio::fs::try_exists(&path).await? {
        tokio::fs::remove_file(&path).await?;
    }
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}
#[derive(Clone, PartialEq, Eq)]
struct AgentView {
    run_id: String,
    label: String,
    details: String,
}
#[derive(Clone, Copy)]
enum Pane {
    Request,
    Session,
    Permissions,
    AgentList,
    AgentDetails,
    Input,
    SettingsList,
    SettingsDetails,
}
#[derive(Clone, Copy, Default)]
struct ScrollPane {
    area: Rect,
    offset: usize,
    max: usize,
    page: usize,
}
impl ScrollPane {
    fn scroll(&mut self, up: bool, rows: usize) {
        self.offset = if up {
            self.offset.saturating_sub(rows)
        } else {
            self.offset.saturating_add(rows).min(self.max)
        };
    }
}
#[derive(Default)]
struct View {
    transcript: String,
    input: String,
    pasted_texts: Vec<text_input::PastedText>,
    paste_number: usize,
    status: String,
    session_id: String,
    run_id: String,
    approval: Option<ApprovalRequest>,
    interaction: Option<InteractionRequest>,
    // None follows the bottom; Some anchors a visual row while output streams.
    scroll: Option<usize>,
    chat_max_scroll: usize,
    chat_page: usize,
    chat_area: Rect,
    panes: [ScrollPane; 8],
    settings: Option<crate::cli_config::Editor>,
    settings_tabs: [Rect; 4],
    settings_apply: Rect,
    settings_scope: Rect,
    settings_discard: Rect,
    discard_armed: bool,
    selection: Option<text_input::Selection>,
    rendered: Option<ratatui::buffer::Buffer>,
    selectable: Vec<Rect>,
    native_selection: bool,
    error: String,
    model_requests: u32,
    tools: u32,
    command_selected: usize,
    commands_dismissed: bool,
    quit_armed: bool,
    agents: String,
    capacity: String,
    request_owner: String,
    context_status: String,
    screen: Screen,
    agent_views: Vec<AgentView>,
    agent_selected: usize,
    permissions: String,
    access_summary: String,
    access_hint: String,
    permission_selected: usize,
    layout: LayoutMode,
    layout_selected: usize,
    choice_selected: usize,
}
impl View {
    async fn open_settings(&mut self, session: Option<&SessionHandle>) {
        match session {
            Some(session) => match crate::cli_config::Editor::open(session).await {
                Ok(editor) => {
                    self.settings = Some(editor);
                    self.screen = Screen::Settings;
                    self.pane(Pane::SettingsList).offset = 0;
                    self.pane(Pane::SettingsDetails).offset = 0;
                    self.error.clear();
                    self.discard_armed = false;
                }
                Err(error) => self.error = error.to_string(),
            },
            None => self.error = "Connect a session before editing settings.".into(),
        }
    }
    async fn save_settings(&mut self, session: Option<&SessionHandle>, close: bool) {
        let (Some(session), Some(editor)) = (session, &mut self.settings) else {
            return;
        };
        let result = editor.apply(session).await;
        if let Err(error) = &result {
            editor.status = error.to_string();
        }
        self.error = result
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        if let Ok(data) = session.snapshot().await {
            self.refresh_permissions(&data);
        }
        if result.is_ok() {
            self.discard_armed = false;
            if close {
                let status = self.settings.as_ref().unwrap().status.clone();
                self.append(&format!("\nSettings · {status}\n"));
                self.settings = None;
                self.screen = Screen::Conversation;
            }
        }
    }

    fn close_settings(&mut self) {
        if let Some(editor) = &mut self.settings
            && editor.is_dirty()
            && !self.discard_armed
        {
            self.discard_armed = true;
            editor.status =
                "UNSAVED changes: F10 applies and closes. Press Esc again to discard.".into();
            return;
        }
        self.settings = None;
        self.screen = Screen::Conversation;
        self.discard_armed = false;
    }
    fn pane(&mut self, pane: Pane) -> &mut ScrollPane {
        &mut self.panes[pane as usize]
    }
    fn paste(&mut self, text: &str) {
        if !matches!(self.screen, Screen::Conversation) {
            return;
        }
        self.selection = None;
        self.quit_armed = false;
        let mut normalized = String::new();
        let complete = text_input::append_paste(&mut normalized, text);
        if !complete || self.expanded_input().len() + normalized.len() > text_input::INPUT_LIMIT {
            self.error = "Paste exceeds the 64 KiB message limit; nothing was added.".into();
            return;
        }
        let lines = normalized.lines().count().max(1);
        if lines >= 5 || normalized.len() > 512 {
            self.paste_number += 1;
            let marker = format!("[Pasted text #{} +{lines} lines]", self.paste_number);
            self.input.push_str(&marker);
            self.pasted_texts.push(text_input::PastedText {
                marker,
                text: normalized,
            });
        } else {
            self.input.push_str(&normalized);
        }
        self.edited_input();
        self.error.clear();
    }
    fn expanded_input(&self) -> String {
        text_input::expand_pastes(&self.input, &self.pasted_texts)
    }
    fn backspace_input(&mut self) {
        if let Some(paste) = self
            .pasted_texts
            .iter()
            .find(|p| self.input.ends_with(&p.marker))
        {
            self.input.truncate(self.input.len() - paste.marker.len());
        } else {
            self.input.pop();
        }
        self.edited_input();
    }
    async fn paste_clipboard(&mut self) {
        if !matches!(self.screen, Screen::Conversation) {
            return;
        }
        match text_input::paste().await {
            Ok(text) => self.paste(&text),
            Err(error) => {
                self.error = format!(
                    "Cannot paste: {error}. Use the terminal paste shortcut (Ctrl-Shift-V)."
                )
            }
        }
    }
    async fn copy_selection(&mut self) -> bool {
        let (Some(selection), Some(buffer)) = (&self.selection, &self.rendered) else {
            return false;
        };
        let text = selection.text(buffer);
        match text_input::copy(&text).await {
            Ok(()) => {
                self.selection = None;
                self.error.clear();
            }
            Err(error) => {
                self.error = format!("Cannot copy: {error}. Use F6 for terminal text selection.")
            }
        }
        self.quit_armed = false;
        true
    }
    fn scroll_at(&mut self, x: u16, y: u16, up: bool) {
        self.selection = None;
        if matches!(self.screen, Screen::Conversation) && self.chat_area.contains((x, y).into()) {
            self.scroll_chat(up, 3);
        } else if let Some(pane) = self
            .panes
            .iter_mut()
            .find(|pane| pane.area.contains((x, y).into()))
        {
            pane.scroll(up, 3);
        }
    }
    fn refresh_permissions(&mut self, data: &SessionData) {
        (self.access_summary, self.permissions) = permission_details(data);
        self.access_hint = if !data.policy.tool_permissions.is_empty()
            || !data.policy.remembered_grants.is_empty()
        {
            " F5 edit access · /permissions reads|writes|process deny|ask|allow [session|project]"
                .into()
        } else if data.policy.request_approvals {
            " Selected tools can request approval · ja/yes approves the displayed action · F2 permissions · F5 settings".into()
        } else {
            " Restricted tool access · F5 settings: edit profile, permissions, tools and skills"
                .into()
        };
    }
    fn scroll_chat(&mut self, up: bool, rows: usize) {
        let position = self.scroll.unwrap_or(self.chat_max_scroll);
        let next = if up {
            position.saturating_sub(rows)
        } else {
            position.saturating_add(rows).min(self.chat_max_scroll)
        };
        self.scroll = (next < self.chat_max_scroll).then_some(next);
    }
    fn show_run_error(&mut self, error: &WorkerError) {
        let advice = match error.code.as_str() {
            "context_overflow" => {
                "The current work or user constraints exceed the context budget. Use /setup for a larger context, or /sessions for a new chat. The saved history is intact."
            }
            "wall_budget" | "step_budget" | "request_budget" | "tool_budget" => {
                "This run reached its limit. Send a follow-up to continue in this session."
            }
            "capability_denied" => {
                "Use F5 to edit workspace writes/process access or /permissions mode manual project, then continue in this session."
            }
            "repetition_limit" => {
                "The agent repeated the same tool call too often. Review blocked rights with F5 or /permissions mode manual project, then send a follow-up. The history is retained."
            }
            _ => "The session is still open. You can send another message.",
        };
        self.append(&format!(
            "\nError · {}: {}\n{advice}\n",
            error.code, error.message
        ));
    }
    fn response_options(&self) -> Vec<String> {
        if let Some(request) = &self.approval {
            let mut options = vec!["Allow once".into(), "Deny".into()];
            if request.session_id == self.session_id {
                let scope = approval_scope(request);
                options.push(format!("Allow {scope} · session"));
                options.push(format!("Allow {scope} · save for project"));
            }
            options
        } else if let Some(question) = &self.interaction {
            let mut options = question.choices.clone();
            if !options.is_empty() {
                options.push("Write my own answer".into());
            }
            options.push("Edit permissions · session / project (F5)".into());
            options
        } else {
            vec![]
        }
    }
    fn open_appearance(&mut self) {
        self.layout_selected = usize::from(self.layout == LayoutMode::Cockpit);
        self.screen = Screen::Appearance;
    }
    fn commands(&self) -> Vec<(&'static str, &'static str)> {
        if self.commands_dismissed || !self.input.starts_with('/') {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .copied()
            .filter(|(name, _)| name.starts_with(&self.input))
            .collect()
    }
    fn edited_input(&mut self) {
        self.pasted_texts
            .retain(|paste| self.input.contains(&paste.marker));
        self.pane(Pane::Input).offset = usize::MAX;
        self.command_selected = 0;
        self.commands_dismissed = false;
    }
    fn append(&mut self, text: &str) {
        self.transcript.push_str(&clean(text));
        if self.transcript.len() > 131_072 {
            let mut boundary = self.transcript.len() - 98_304;
            while !self.transcript.is_char_boundary(boundary) {
                boundary += 1;
            }
            self.transcript.drain(..boundary);
        }
    }
    fn event(&mut self, event: WorkerEvent) {
        if let Some(id) = event.run_id {
            self.run_id = id;
        }
        match event.event {
            EventKind::TextDelta { text, .. } => self.append(&text),
            EventKind::Subagent { goal, status, .. } => {
                self.append(&format!("\n  agent · {status:?} · {goal}\n"))
            }
            EventKind::ContextPlanned { snapshot } => {
                self.context_status = format!(
                    "Context estimate: {} · omitted: {}",
                    snapshot.estimated_tokens,
                    snapshot.omitted_messages.len()
                );
                if !snapshot.omitted_messages.is_empty() {
                    self.append(&format!(
                        "\n  compaction · {} messages represented by bounded excerpts/summary\n",
                        snapshot.omitted_messages.len()
                    ));
                }
            }
            EventKind::ModelRequest { .. } => {
                self.model_requests += 1;
                self.append("\n\nwerk › ");
            }
            EventKind::AttemptFailed { code, .. } => {
                self.append(&format!("\n[Attempt interrupted: {code}]\n"))
            }
            EventKind::ToolPrepared { name, .. } => {
                self.tools += 1;
                self.append(&format!("\n  tool · {name}\n"));
            }
            EventKind::ToolCompleted { result, .. } => self.append(&format!(
                "  result · {:?} {}\n",
                result.status,
                result.error_code.unwrap_or_default()
            )),
            // Pending requests and their addressed run are refreshed together from
            // snapshots. Root events must not replace a displayed child request.
            EventKind::RunState { status } => {
                self.status = format!("{status:?}");
                if status.terminal() {
                    self.approval = None;
                    self.interaction = None;
                    self.append(&format!("\n[{status:?}]\n"));
                }
            }
            _ => {}
        }
    }
}

fn permission_details(data: &SessionData) -> (String, String) {
    let summary = format!(
        "Profile: {}\nWorkspace writes: {}\nProcess tool: {}",
        data.profile.name,
        if !data.policy.workspace_writable {
            "disabled · F5 to edit"
        } else if data.policy.tool_permissions.get("files.write")
            == Some(&policy::ToolPermission::Allow)
            || data
                .policy
                .remembered_grants
                .iter()
                .any(|g| g.tool == "files.write" && g.arguments_hash.is_none())
        {
            "allowed in workspace"
        } else if data.policy.request_approvals
            || data.policy.tool_permissions.get("files.write") == Some(&policy::ToolPermission::Ask)
        {
            "explicit approval required"
        } else {
            "enabled (within grants)"
        },
        if let Some(permission) = data.policy.tool_permissions.get("process.run") {
            match permission {
                policy::ToolPermission::Deny => "blocked",
                policy::ToolPermission::Ask => "sandboxed; ask",
                policy::ToolPermission::Allow => "sandboxed; allowed",
            }
        } else if data.profile.tools.iter().any(|t| t == "process.run") {
            if data.policy.request_approvals {
                "sandboxed; explicit approval required"
            } else if data.policy.grants.iter().any(|g| g.tool == "process.run") {
                "granted; see /permissions"
            } else {
                "not granted"
            }
        } else {
            "not in profile"
        },
    );
    let grants = data
        .policy
        .grants
        .iter()
        .chain(data.policy.remembered_grants.iter())
        .map(|g| {
            format!(
                "{} · {} · {}{}{}",
                g.tool,
                g.resource,
                if g.preapproved || data.policy.mode == policy::ApprovalMode::FullAccess {
                    "preapproved"
                } else {
                    "ask each time"
                },
                if g.arguments_hash.is_some() {
                    " · exact arguments only"
                } else {
                    ""
                },
                g.expires_at_ms
                    .map(|t| format!(" · expires at {t} ms"))
                    .unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let rules = data
        .policy
        .tool_permissions
        .iter()
        .map(|(tool, rule)| format!("{tool}: {rule:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    let details = format!(
        "E / F5: edit profile, permissions, tools and skills\nEnter: review enabling all tools with explicit approval\n\n{summary}\nWorkspace: {}\n\nAvailable profile tools:\n{}\n\nHost grants:\n{}\n\nHard-denied tools: {}\n\nAvailable skills: {}\n\nApproval requests expose the actual tool, file diff or command before execution.\nChoose Allow once, Ctrl-Y, or type ja/yes to approve that action; Deny, Ctrl-N or nein/no rejects it.\nA normal agent question is not an action approval.\nNew subagents inherit access and ask for their own addressed approvals.\nFinish or cancel existing subagents before changing this session's access.",
        data.workspace.display(),
        data.profile
            .tools
            .chunks(3)
            .map(|names| names.join(", "))
            .collect::<Vec<_>>()
            .join("\n"),
        format_args!("{grants}\n{rules}"),
        if data.policy.hard_denied_tools.is_empty() {
            "None".into()
        } else {
            data.policy.hard_denied_tools.join(", ")
        },
        data.profile.skills.join(", "),
    );
    (summary, details)
}

fn agent_details(run: &Run, goal: &str) -> String {
    let mut view = View::default();
    view.append(&format!(
        "Goal: {goal}\nRun: {}\nStatus: {:?}\n\n",
        run.id, run.status
    ));
    if let Some(text) = &run.final_text {
        view.append(&format!("Final response:\n{text}\n\n"));
    }
    if let Some(error) = &run.error {
        view.append(&format!(
            "Run failed: {} · {}\n\n",
            error.code, error.message
        ));
    }
    for attempt in &run.attempts {
        view.append(&format!("werk > {}\n", attempt.partial_text));
        if let Some(error) = &attempt.error {
            view.append(&format!("Attempt error: {}\n", error.code));
        }
        for invocation in run
            .invocations
            .iter()
            .filter(|i| i.step_id == attempt.step_id)
        {
            view.append(&format!(
                "\nTool: {}\n{}\n",
                invocation.call.name,
                serde_json::to_string_pretty(&invocation.call.arguments).unwrap_or_default()
            ));
            if let Some(request) = &invocation.approval {
                view.append(&format!(
                    "Approval: {}\n",
                    match request.decision {
                        None => "waiting (Esc to review)",
                        Some(ApprovalDecision::Deny) => "denied",
                        _ => "allowed",
                    }
                ));
            }
            if let Some(request) = &invocation.interaction {
                view.append(&format!(
                    "Question: {}\nAnswer: {}\n",
                    request.question,
                    request
                        .answer
                        .as_deref()
                        .unwrap_or("waiting (Esc to answer)")
                ));
            }
            if let Some(result) = &invocation.result {
                view.append(&format!(
                    "Result: {:?} {}\n{}\n",
                    result.status,
                    result.error_code.as_deref().unwrap_or(""),
                    serde_json::to_string_pretty(&result.data).unwrap_or_default()
                ));
            }
        }
    }
    view.transcript
}

pub(crate) fn panel(title: &str, color: Color) -> Block<'_> {
    Block::default()
        .title(Line::styled(title, Style::default().fg(color).bold()))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(PANEL).fg(TEXT))
}
fn wrapped_lines(text: &str, width: u16) -> usize {
    Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .line_count(width.max(1))
}

fn draw_scrolled(frame: &mut Frame, state: &mut ScrollPane, paragraph: Paragraph<'_>, area: Rect) {
    state.area = area;
    state.page = usize::from(area.height).max(1);
    state.max = paragraph
        .line_count(area.width.max(1))
        .saturating_sub(area.height as usize)
        .min(u16::MAX as usize);
    state.offset = state.offset.min(state.max);
    frame.render_widget(paragraph.scroll((state.offset as u16, 0)), area);
}

fn draw_text_panel(
    frame: &mut Frame,
    view: &mut View,
    pane: Pane,
    text: String,
    title: &str,
    color: Color,
    area: Rect,
) {
    let block = panel(title, color);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    view.selectable.push(inner);
    let text = if matches!(
        pane,
        Pane::Permissions | Pane::Session | Pane::SettingsDetails
    ) {
        information_text(&text)
    } else {
        Text::from(text)
    };
    draw_scrolled(
        frame,
        view.pane(pane),
        Paragraph::new(text).wrap(Wrap { trim: false }),
        inner,
    );
}

fn draw_request(frame: &mut Frame, view: &mut View, area: Rect) {
    let (title, body) = if let Some(request) = &view.approval {
        (
            " Approval ",
            format!(
                "Agent: {}\nTool: {}\n{}\n\nApproves only this action. Sandbox and hard denies still apply.",
                view.request_owner,
                request.description["tool"].as_str().unwrap_or("unknown"),
                serde_json::to_string_pretty(&request.description).unwrap_or_default()
            ),
        )
    } else if let Some(request) = &view.interaction {
        (
            " Question ",
            format!("Agent: {}\n\n{}", view.request_owner, request.question),
        )
    } else {
        return;
    };
    let block = panel(title, BRAND[4]);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let options = view.response_options();
    let option_height = (options.len().min(4) as u16).min(inner.height.saturating_sub(2));
    let rows = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(option_height),
        Constraint::Length(1),
    ])
    .split(inner);
    view.selectable.push(rows[0]);
    draw_scrolled(
        frame,
        view.pane(Pane::Request),
        Paragraph::new(clean(&body)).wrap(Wrap { trim: false }),
        rows[0],
    );
    let selected = view.choice_selected.min(options.len().saturating_sub(1));
    let first = (selected + 1).saturating_sub(option_height as usize);
    let choices: Vec<Line<'_>> = options
        .iter()
        .enumerate()
        .skip(first)
        .map(|(i, option)| {
            choice_line(
                format!(
                    "{} {}",
                    if i == selected && view.input.is_empty() {
                        "›"
                    } else {
                        " "
                    },
                    clean(option)
                ),
                i == selected && view.input.is_empty(),
                rows[1].width,
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(choices), rows[1]);
    let hint = if view.approval.is_some() {
        "↑↓ choose · Enter confirm · Ctrl-Y allow / Ctrl-N deny · Alt-↑↓ details"
    } else if options.is_empty() {
        "Type your answer below · Enter send · Alt-↑↓ details"
    } else {
        "↑↓ choose · Enter send · or type your own answer · Alt-↑↓ details"
    };
    frame.render_widget(Paragraph::new(shortcut_text(hint)), rows[2]);
}

fn draw_chat(frame: &mut Frame, view: &mut View, area: Rect) {
    let pending = view.approval.is_some() || view.interaction.is_some();
    let (chat, request) = if pending {
        let body = view
            .interaction
            .as_ref()
            .map(|q| q.question.as_str())
            .unwrap_or("");
        let desired = if view.approval.is_some() {
            14
        } else {
            wrapped_lines(body, area.width.saturating_sub(2))
                + view.response_options().len().min(4)
                + 7
        };
        let height = (desired.min(u16::MAX as usize) as u16).min(area.height.saturating_mul(2) / 3);
        let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(height)]).split(area);
        (rows[0], Some(rows[1]))
    } else {
        (area, None)
    };
    let height = usize::from(chat.height.saturating_sub(2));
    let paragraph = Paragraph::new(crate::cli_transcript::render(
        &view.transcript,
        chat.width.saturating_sub(2),
    ))
    .wrap(Wrap { trim: false });
    let offset = paragraph
        .line_count(chat.width.saturating_sub(2))
        .saturating_sub(height)
        .min(u16::MAX as usize);
    view.chat_max_scroll = offset;
    view.chat_page = height.max(1);
    view.chat_area = chat;
    view.selectable.push(panel("", MUTED).inner(chat));
    if let Some(position) = &mut view.scroll {
        *position = (*position).min(offset);
    }
    let position = view.scroll.unwrap_or(offset);
    let title = if view.scroll.is_some() {
        format!(
            " Conversation · {}/{} · End: latest ",
            position + 1,
            offset + 1
        )
    } else {
        " Conversation ".into()
    };
    frame.render_widget(
        paragraph.scroll((position as u16, 0)).block(panel(
            &title,
            if view.layout == LayoutMode::Compact {
                MUTED
            } else {
                BRAND[0]
            },
        )),
        chat,
    );
    if offset > 0 && chat.height > 2 {
        let mut state = ScrollbarState::new(offset + 1)
            .position(position)
            .viewport_content_length(height);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .thumb_style(Style::default().fg(MUTED)),
            Rect::new(chat.x, chat.y + 1, chat.width, chat.height - 2),
            &mut state,
        );
    }
    if let Some(area) = request {
        draw_request(frame, view, area);
    }
}

fn draw_agents(frame: &mut Frame, view: &mut View, area: Rect) {
    let split = if view.layout == LayoutMode::Cockpit {
        Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).split(area)
    } else {
        Layout::vertical([
            Constraint::Length(
                (view.agent_views.len().clamp(1, 5) as u16 + 2).min(area.height / 3),
            ),
            Constraint::Min(1),
        ])
        .split(area)
    };
    let items = if view.agent_views.is_empty() {
        vec![Line::from("No subagents in this run yet.")]
    } else {
        view.agent_views
            .iter()
            .enumerate()
            .map(|(i, agent)| {
                choice_line(
                    format!(
                        "{} {}",
                        if i == view.agent_selected { ">" } else { " " },
                        clean(&agent.label)
                    ),
                    i == view.agent_selected,
                    split[0].width.saturating_sub(2),
                )
            })
            .collect()
    };
    let block = panel(" Subagents · Up/Down select ", BRAND[4]);
    let inner = block.inner(split[0]);
    frame.render_widget(block, split[0]);
    view.selectable.push(inner);
    draw_scrolled(
        frame,
        view.pane(Pane::AgentList),
        Paragraph::new(items),
        inner,
    );
    let details = view
        .agent_views
        .get(view.agent_selected)
        .map(|a| a.details.as_str())
        .unwrap_or(
            "Subagent progress and tool results appear here.\nEsc returns to the conversation.",
        )
        .to_string();
    draw_text_panel(
        frame,
        view,
        Pane::AgentDetails,
        clean(&details),
        " Agent details · PgUp/PgDn scroll · Esc close ",
        BRAND[3],
        split[1],
    );
}

fn draw_settings(frame: &mut Frame, view: &mut View, area: Rect) {
    let Some(editor) = &view.settings else {
        return;
    };
    let tab = editor.tab;
    let selected = editor.selected[tab];
    let details = editor.details();
    let status = clean(&format!(
        "{}{}",
        if editor.is_dirty() { "UNSAVED · " } else { "" },
        editor.status
    ));
    let save_project = editor.save_project;
    let items = editor.items();
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .split(area);
    let mut x = rows[0].x;
    for (index, title) in crate::cli_config::TABS.iter().enumerate() {
        let width = (title.len() as u16 + 2).min(rows[0].right().saturating_sub(x));
        view.settings_tabs[index] = Rect::new(x, rows[0].y, width, 1);
        x += width;
    }
    frame.render_widget(
        Paragraph::new(Line::from(
            crate::cli_config::TABS
                .iter()
                .enumerate()
                .map(|(i, title)| Span::styled(format!(" {title} "), choice_style(i == tab)))
                .collect::<Vec<_>>(),
        )),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(status)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(BRAND[0])),
        rows[1],
    );
    let split = if area.width >= 80 {
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).split(rows[2])
    } else {
        Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).split(rows[2])
    };
    let block = panel(crate::cli_config::TABS[tab], BRAND[4]);
    let inner = block.inner(split[0]);
    frame.render_widget(block, split[0]);
    let items = items
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            choice_line(
                format!("{} {}", if i == selected { "›" } else { " " }, clean(&text)),
                i == selected,
                inner.width,
            )
        })
        .collect::<Vec<_>>();
    draw_scrolled(
        frame,
        view.pane(Pane::SettingsList),
        Paragraph::new(items),
        inner,
    );
    draw_text_panel(
        frame,
        view,
        Pane::SettingsDetails,
        clean(&details),
        " Settings · details ",
        BRAND[3],
        split[1],
    );
    let controls = [
        " Apply & close [F10] ".to_string(),
        if save_project {
            " Session + project [P] "
        } else {
            " Session only [P] "
        }
        .to_string(),
        " Discard / close [Esc] ".to_string(),
    ];
    let mut x = rows[3].x;
    for (index, label) in controls.iter().enumerate() {
        let width = (label.len() as u16).min(rows[3].right().saturating_sub(x));
        let area = Rect::new(x, rows[3].y, width, 1);
        match index {
            0 => view.settings_apply = area,
            1 => view.settings_scope = area,
            _ => view.settings_discard = area,
        }
        frame.render_widget(
            Paragraph::new(Line::styled(label.clone(), choice_style(index == 0))),
            area,
        );
        x += width;
    }
    frame.render_widget(Paragraph::new(shortcut_text("Click tabs/options to change · Tab sections · ↑/↓ select · Enter change\nCtrl-S save · F10 apply & close")), Rect::new(rows[3].x, rows[3].y + 1, rows[3].width, rows[3].height.saturating_sub(1)));
}

fn draw(frame: &mut Frame, view: &mut View) {
    let area = frame.area();
    // Keep the selected text stable while model output continues in the background.
    if let (Some(selection), Some(buffer)) = (&view.selection, &view.rendered)
        && buffer.area == area
    {
        *frame.buffer_mut() = buffer.clone();
        selection.highlight(frame.buffer_mut());
        return;
    }
    view.selection = None;
    view.selectable.clear();
    for pane in &mut view.panes {
        pane.area = Rect::default();
    }
    frame.render_widget(
        Block::default().style(Style::default().bg(BG).fg(TEXT)),
        area,
    );
    let wide_banner = view.layout == LayoutMode::Cockpit && area.width >= 94 && area.height >= 24;
    let rows = Layout::vertical([
        Constraint::Length(if wide_banner { 8 } else { 2 }),
        Constraint::Min(1),
        Constraint::Length(
            (wrapped_lines(&format!("› {}", view.input), area.width.saturating_sub(2)) as u16)
                .clamp(1, 4)
                + 2,
        ),
        Constraint::Length(2),
    ])
    .split(area);
    if wide_banner {
        frame.render_widget(Paragraph::new(banner()), rows[0]);
    } else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    format!(
                        " werkWorker · {} · {}",
                        if view.layout == LayoutMode::Compact {
                            "Compact"
                        } else {
                            "Cockpit"
                        },
                        view.status
                    ),
                    Style::default().fg(BRAND[0]).bold(),
                ),
                Line::styled(
                    clean(&format!(
                        " {} · {} subagents · {} model requests · {} tool calls",
                        view.capacity.lines().next().unwrap_or("Not connected"),
                        view.agent_views.len(),
                        view.model_requests,
                        view.tools
                    )),
                    Style::default().fg(MUTED),
                ),
            ]),
            rows[0],
        );
    }
    match view.screen {
        Screen::Conversation => {
            if view.layout == LayoutMode::Cockpit {
                let columns =
                    Layout::horizontal([Constraint::Percentage(68), Constraint::Percentage(32)])
                        .split(rows[1]);
                draw_chat(frame, view, columns[0]);
                let details = format!(
                    "Status: {}\n{}\n{}\n\n{}\n{}\n\n{}\n\n{}\n\nModel requests: {}\nTool calls: {}\n\nF2 permissions · F3 agents\nF4 view menu · F5 settings",
                    view.status,
                    view.session_id,
                    view.run_id,
                    view.capacity,
                    view.context_status,
                    view.agents,
                    view.access_summary,
                    view.model_requests,
                    view.tools
                );
                draw_text_panel(
                    frame,
                    view,
                    Pane::Session,
                    clean(&details),
                    " Session ",
                    BRAND[3],
                    columns[1],
                );
            } else {
                draw_chat(frame, view, rows[1]);
            }
        }
        Screen::Agents => draw_agents(frame, view, rows[1]),
        Screen::Settings => draw_settings(frame, view, rows[1]),
        Screen::Permissions => {
            draw_text_panel(
                frame,
                view,
                Pane::Permissions,
                clean(&view.permissions),
                " Permissions · PgUp/PgDn scroll · Esc close ",
                BRAND[3],
                rows[1],
            );
        }
        Screen::PermissionConsent => {
            let mut lines = vec![
                Line::styled(
                    "Enable tool approval requests in this session?",
                    Style::default().fg(HEADING).bold(),
                ),
                Line::from(""),
                Line::from("Make all registered tools and skills available to the agent."),
                Line::from("File changes, sandbox commands and network actions each need"),
                Line::from("your explicit approval of the actual action before execution."),
                Line::from("The workspace and hard-denied tools remain unchanged."),
                Line::from("New subagents inherit this mode and ask for their own actions."),
                Line::from("This setting is saved with the session; it executes nothing."),
                Line::from(""),
            ];
            for (i, label) in [
                "Enable all tools; ask before actions",
                "Keep current permissions",
            ]
            .iter()
            .enumerate()
            {
                lines.push(choice_line(
                    format!(
                        "{} {label}",
                        if i == view.permission_selected {
                            "›"
                        } else {
                            " "
                        }
                    ),
                    i == view.permission_selected,
                    rows[1].width.saturating_sub(2),
                ));
            }
            lines.extend(shortcut_text("\n↑/↓ select · Enter confirm · Esc cancel").lines);
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(panel(" Permissions · confirmation ", BRAND[4])),
                rows[1],
            );
        }
        Screen::Appearance => {
            let labels = [
                "Compact — one chat column, inline questions and answers",
                "Cockpit — conversation with a session sidebar",
            ];
            let mut lines = vec![
                Line::styled("Choose your layout", Style::default().fg(HEADING).bold()),
                Line::from(""),
            ];
            for (i, label) in labels.iter().enumerate() {
                lines.push(choice_line(
                    format!(
                        "{} {label}",
                        if i == view.layout_selected {
                            "›"
                        } else {
                            " "
                        }
                    ),
                    i == view.layout_selected,
                    rows[1].width.saturating_sub(2),
                ));
                lines.push(Line::from(""));
            }
            lines.extend(
                shortcut_text("Up/Down: select · Enter: apply and remember · Esc: cancel").lines,
            );
            lines.push(Line::from(
                "Your conversation and pending answers stay in place.",
            ));
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(panel(" View · Compact / Cockpit ", BRAND[4])),
                rows[1],
            );
        }
    }
    let pending = view.approval.is_some() || view.interaction.is_some();
    let input_title = if !matches!(view.screen, Screen::Conversation) {
        " Inspector · Esc returns to chat / pending request "
    } else if view.approval.is_some() {
        " Approval · ↑↓ choose or type ja/yes / nein/no · Enter "
    } else if view.interaction.is_some() {
        if view
            .interaction
            .as_ref()
            .is_some_and(|q| q.choices.is_empty() || view.choice_selected == q.choices.len())
        {
            " Your answer · type below · Enter send "
        } else {
            " Your answer · ↑↓ choose or type · Enter send "
        }
    } else {
        " Message · Enter to send "
    };
    draw_text_panel(
        frame,
        view,
        Pane::Input,
        format!("› {}", view.input),
        input_title,
        if pending { BRAND[4] } else { BRAND[1] },
        rows[2],
    );
    let footer = if view.quit_armed {
        " Press Ctrl-C again to quit. Any other key continues."
    } else if !view.error.is_empty() {
        &view.error
    } else if view.native_selection {
        " Terminal selection: drag to select · terminal copy/paste shortcuts · F6: return to pane scrolling"
    } else {
        " Ctrl-C copy selection · Ctrl-V paste · Right-click copy/paste · Wheel scroll pane · F6 terminal selection · /help"
    };
    frame.render_widget(
        Paragraph::new(if view.error.is_empty() {
            shortcut_text(&format!("{footer}\n{}", view.access_hint))
        } else {
            Text::styled(
                format!("{}\n{}", clean(footer), view.access_hint),
                Style::default().fg(Color::Rgb(255, 120, 130)).bold(),
            )
        }),
        rows[3],
    );
    let commands = if matches!(view.screen, Screen::Conversation) {
        view.commands()
    } else {
        vec![]
    };
    if !commands.is_empty() {
        let height = (commands.len() as u16 + 2).min(rows[2].y.saturating_sub(area.y));
        if height >= 3 {
            let popup = Rect::new(area.x, rows[2].y - height, area.width.min(80), height);
            let selected = view.command_selected.min(commands.len() - 1);
            let start = (selected + 1).saturating_sub(usize::from(height - 2));
            let items: Vec<Line<'_>> = commands
                .iter()
                .enumerate()
                .skip(start)
                .map(|(i, (name, description))| {
                    choice_line(
                        format!(
                            "{} {name:<13} {description}",
                            if i == selected { "›" } else { " " }
                        ),
                        i == selected,
                        popup.width.saturating_sub(2),
                    )
                })
                .collect();
            frame.render_widget(Clear, popup);
            frame.render_widget(
                Paragraph::new(items).block(panel(
                    " Commands · ↑↓ select · Tab complete · Enter run · Esc close ",
                    BRAND[4],
                )),
                popup,
            );
            view.selectable.insert(0, panel("", MUTED).inner(popup));
        }
    }
    view.rendered = Some(frame.buffer_mut().clone());
}

// Surface the first pending addressed request across the bounded agent tree.
// Approval keys remain attached to the child run; no parent approval is reused.
async fn refresh_agents(root: &RunHandle, view: &mut View) -> Result<Option<RunHandle>> {
    let mut pending = vec![(root.clone(), "Main agent".to_owned(), 0usize)];
    let mut lines = Vec::new();
    let selected = view
        .agent_views
        .get(view.agent_selected)
        .map(|a| a.run_id.clone());
    let mut agent_views = Vec::new();
    let mut addressed = None;
    let mut approval = None;
    let mut interaction = None;
    let mut owner = String::new();
    while let Some((run, goal, depth)) = pending.pop() {
        let state = run.status().await?;
        if depth > 0 {
            lines.push(format!(
                "{}{:?} · {}",
                "  ".repeat(depth - 1),
                state.status,
                clean(&goal).chars().take(80).collect::<String>()
            ));
            agent_views.push(AgentView {
                run_id: state.id.clone(),
                label: clean(&format!(
                    "{}{:?} · {goal}",
                    "  ".repeat(depth - 1),
                    state.status
                )),
                details: agent_details(&state, &goal),
            });
        }
        if addressed.is_none()
            && matches!(
                state.status,
                RunStatus::WaitingApproval | RunStatus::WaitingInput
            )
        {
            for invocation in &state.invocations {
                if invocation.result.is_some() {
                    continue;
                }
                if let Some(a) = invocation
                    .approval
                    .as_ref()
                    .filter(|a| a.decision.is_none())
                {
                    approval = Some(a.clone());
                    addressed = Some(run.clone());
                    owner = goal.clone();
                    break;
                }
                if let Some(q) = invocation
                    .interaction
                    .as_ref()
                    .filter(|q| q.answer.is_none())
                {
                    interaction = Some(q.clone());
                    addressed = Some(run.clone());
                    owner = goal.clone();
                    break;
                }
            }
        }
        for child in run.subagents().await?.into_iter().rev() {
            if child.run.is_some() {
                pending.push((
                    run.subagent(&child.session_id).await?,
                    child.goal,
                    depth + 1,
                ));
            } else {
                lines.push(format!("Interrupted · {}", child.goal));
                agent_views.push(AgentView {
                    run_id: child.session_id.clone(),
                    label: clean(&format!("Interrupted · {}", child.goal)),
                    details: clean(&format!(
                        "{}\nSession: {}\nInterrupted before the run started.",
                        child.goal, child.session_id
                    )),
                });
            }
        }
    }
    view.agents = if lines.is_empty() {
        "Subagents: none".into()
    } else {
        format!("Subagents:\n{}", lines.join("\n"))
    };
    view.agent_selected = selected
        .and_then(|id| agent_views.iter().position(|a| a.run_id == id))
        .unwrap_or(0);
    view.agent_views = agent_views;
    let changed = view.approval.as_ref().map(|a| &a.request_id)
        != approval.as_ref().map(|a| &a.request_id)
        || view.interaction.as_ref().map(|q| &q.request_id)
            != interaction.as_ref().map(|q| &q.request_id);
    if changed {
        view.pane(Pane::Request).offset = 0;
        view.choice_selected = usize::from(approval.is_some());
        if interaction.is_some() {
            view.input.clear();
            view.edited_input();
        }
    }
    view.approval = approval;
    view.interaction = interaction;
    view.request_owner = owner;
    Ok(addressed)
}

async fn answer_question(view: &mut View, run: &RunHandle, answer: &str) -> Result<()> {
    if let Some(request) = view.interaction.clone() {
        run.respond_to_interaction(&request.request_id, answer, &id("answer"))
            .await?;
        let display_answer = if view.input.is_empty() {
            answer.to_owned()
        } else {
            view.input.clone()
        };
        view.append(&format!(
            "\n\n{} asks > {}\nyou › {display_answer}\n",
            view.request_owner, request.question
        ));
        view.interaction = None;
        view.input.clear();
        view.edited_input();
        view.scroll = None;
    }
    Ok(())
}

fn approval_answer(input: &str) -> Option<bool> {
    match input.trim().to_lowercase().as_str() {
        "ja" | "j" | "yes" | "y" | "allow" | "erlauben" => Some(true),
        "nein" | "n" | "no" | "deny" | "ablehnen" => Some(false),
        _ => None,
    }
}

fn approval_scope(request: &ApprovalRequest) -> String {
    let tool = request.description["tool"].as_str().unwrap_or("tool");
    match request.description["effect"].as_str() {
        Some("read" | "write") => format!("{tool} in workspace"),
        _ => "exact prepared action".into(),
    }
}

async fn approve_action(view: &mut View, run: &RunHandle, allow: bool) -> Result<()> {
    approve_choice(view, run, usize::from(!allow), None).await
}

async fn approve_choice(
    view: &mut View,
    run: &RunHandle,
    choice: usize,
    session: Option<&SessionHandle>,
) -> Result<()> {
    if let Some(request) = view.approval.clone() {
        run.respond_to_approval(
            &request.request_id,
            match choice {
                0 => ApprovalDecision::AllowOnce,
                1 => ApprovalDecision::Deny,
                _ => ApprovalDecision::AllowForSession,
            },
            &id("approval"),
        )
        .await?;
        let tool = request.description["tool"].as_str().unwrap_or("action");
        view.append(&format!(
            "\n\n{} · {tool}: {}\n",
            view.request_owner,
            match choice {
                0 => "Allowed once",
                1 => "Denied",
                _ => "Allowed and remembered for this session",
            }
        ));
        view.approval = None;
        view.input.clear();
        view.edited_input();
        if choice == 3
            && let Some(session) = session
        {
            crate::cli_config::save_defaults(session).await.map_err(|error| WorkerError::new(
                "defaults_save_failed", format!("Action approved and remembered in this session, but project defaults could not be saved: {error}"),
            ))?;
            view.append("Saved permission in project defaults.\n");
        }
        if let Some(session) = session {
            view.refresh_permissions(&session.snapshot().await?);
        }
    }
    Ok(())
}

pub enum Exit {
    Quit,
    Setup,
    Sessions,
}

fn restore_history(view: &mut View, data: &SessionData) {
    for (index, message) in data.messages.iter().enumerate() {
        match message.role {
            Role::User => view.append(&format!("\n\nyou › {}\n", message.content)),
            Role::Assistant => {
                if !message.content.is_empty() {
                    view.append(&format!("\n\nwerk › {}\n", message.content));
                }
                for call in &message.tool_calls {
                    view.append(&format!("\n  tool · {}\n", call.name));
                }
            }
            Role::Tool => {
                let invocation = data
                    .runs
                    .iter()
                    .flat_map(|r| &r.invocations)
                    .filter(|i| {
                        i.message_index < index && Some(&i.call.id) == message.tool_call_id.as_ref()
                    })
                    .max_by_key(|i| i.message_index);
                if let Some(question) = invocation.and_then(|i| i.interaction.as_ref()) {
                    view.append(&format!("\n  question · {}\n", question.question));
                    if let Some(answer) = &question.answer {
                        view.append(&format!("you › {answer}\n"));
                    }
                } else if let Some(result) = invocation.and_then(|i| i.result.as_ref()) {
                    view.append(&format!(
                        "  result · {:?} {}\n",
                        result.status,
                        result.error_code.as_deref().unwrap_or_default()
                    ));
                }
            }
            _ => {}
        }
    }
    if let Some(run) = data.runs.last() {
        view.run_id = run.id.clone();
        view.status = format!("{:?}", run.status);
        for attempt in run
            .attempts
            .iter()
            .filter(|a| !a.completed && !a.partial_text.is_empty())
        {
            view.append(&format!(
                "\nwerk › {}\n[Unfinished attempt]\n",
                attempt.partial_text
            ));
        }
        view.append(&format!("\n[Opened saved session · {:?}]\n", run.status));
        if let Some(error) = &run.error {
            view.show_run_error(error);
        }
    }
    view.model_requests = data
        .events
        .iter()
        .filter(|e| matches!(e.event, EventKind::ModelRequest { .. }))
        .count() as u32;
    view.tools = data
        .events
        .iter()
        .filter(|e| matches!(e.event, EventKind::ToolPrepared { .. }))
        .count() as u32;
}

pub async fn show(
    session: Option<SessionHandle>,
    initial: Option<String>,
    resume: Option<RunHandle>,
    limits: Limits,
) -> Result<Exit> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(WorkerError::new(
            "terminal_required",
            "TUI requires an interactive terminal; use run or rpc for pipes",
        ));
    }
    let mut view = View {
        status: "Ready".into(),
        ..Default::default()
    };
    match load_layout().await {
        Ok(layout) => view.layout = layout,
        Err(error) => {
            view.error = format!(
                "Cannot read view preferences ({}); using Compact.",
                error.code
            )
        }
    }
    if let Some(session) = &session {
        let execution = session.execution_status();
        view.capacity = format!(
            "Inference slots: {}\n{}",
            execution["effective_model_requests"],
            execution["inference"]["detail"].as_str().unwrap_or("")
        );
    }
    let mut active = resume;
    let mut cursor = 0;
    if let Some(session) = &session {
        let data = session.snapshot().await?;
        cursor = data.events.last().map_or(0, |e| e.cursor);
        restore_history(&mut view, &data);
        view.refresh_permissions(&data);
        view.session_id = data.id;
        // refresh_agents exposes saved requests only once recovery has returned
        // the owning run to WaitingApproval/WaitingInput, not while still queued.
        if let Some(input) = initial {
            if let Some(run) = &active
                && run.status().await?.status == RunStatus::Interrupted
            {
                view.input = input;
                view.error = RESUME_REQUIRED.into();
            } else {
                view.append(&format!("you › {input}\n"));
                active = Some(
                    session
                        .submit(input, &id("tui"), limits.clone(), None)
                        .await?,
                );
            }
        }
    } else {
        view.append("Welcome to werkWorker.\n\nConnect Werk to start an agent, or choose the explicit offline demo.\nThe connection commands are shown in the session panel.");
        view.status = "Setup".into();
    }
    let mut events: Pin<Box<dyn Stream<Item = Result<WorkerEvent>> + Send>> = match &session {
        Some(session) => Box::pin(session.events(cursor)),
        None => Box::pin(futures_util::stream::pending()),
    };
    let mut terminal = ratatui::try_init()?;
    let _restore = Restore;
    crossterm::execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let mut keys = input().await;
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut dirty = true;
    let mut agents_tick = tokio::time::interval(Duration::from_millis(250));
    let mut addressed: Option<RunHandle> = None;
    let mut exit = Exit::Quit;
    loop {
        tokio::select! {
            _ = agents_tick.tick() => {
                if let Some(run) = &active {
                    let previous = (view.agents.clone(), view.approval.as_ref().map(|a|a.request_id.clone()), view.interaction.as_ref().map(|a|a.request_id.clone()), view.agent_views.clone());
                    addressed = refresh_agents(run, &mut view).await?;
                    dirty |= previous != (view.agents.clone(), view.approval.as_ref().map(|a|a.request_id.clone()), view.interaction.as_ref().map(|a|a.request_id.clone()), view.agent_views.clone());
                }
            },
            _ = tick.tick() => { if dirty { terminal.draw(|frame| draw(frame, &mut view))?; dirty = false; } },
            event = events.next() => match event {
                Some(Ok(event)) => {
                    let ended = matches!(event.event, EventKind::RunState { status } if status.terminal());
                    let run_id = event.run_id.clone();
                    view.event(event);
                    if ended && let (Some(session), Some(run_id)) = (&session, run_id) {
                        let data = session.snapshot().await?;
                        if let Some(error) = &data.run(&run_id)?.error {
                            view.show_run_error(error);
                        }
                    }
                    dirty = true;
                },
                Some(Err(error)) => return Err(error),
                None => break,
            },
            key = keys.next() => {
                let Some(key) = key else { break; };
                dirty = true;
                let event = key?;
                if let Event::Paste(text) = &event {
                    view.paste(text);
                    continue;
                }
                if let Event::Mouse(mouse) = event {
                    match mouse.kind {
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                            view.scroll_at(mouse.column, mouse.row, mouse.kind == MouseEventKind::ScrollUp);
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            if matches!(view.screen, Screen::Settings) {
                                view.selection = None;
                                let point = (mouse.column, mouse.row).into();
                                if view.settings_apply.contains(point) {
                                    view.save_settings(session.as_ref(), true).await;
                                    continue;
                                }
                                if view.settings_discard.contains(point) {
                                    view.close_settings();
                                    continue;
                                }
                                if view.settings_scope.contains(point) {
                                    if let Some(editor) = &mut view.settings { editor.save_project = !editor.save_project; }
                                    continue;
                                }
                                if let Some(tab) = view.settings_tabs.iter().position(|area| area.contains(point)) {
                                    if let Some(editor) = &mut view.settings { editor.tab = tab; }
                                    view.pane(Pane::SettingsList).offset = 0;
                                    view.pane(Pane::SettingsDetails).offset = 0;
                                    view.selection = None;
                                    continue;
                                }
                            }
                            let list = *view.pane(Pane::SettingsList);
                            if matches!(view.screen, Screen::Settings) && list.area.contains((mouse.column, mouse.row).into()) {
                                if let Some(editor) = &mut view.settings {
                                    let selected = list.offset + usize::from(mouse.row - list.area.y);
                                    if selected < editor.items().len() {
                                        editor.selected[editor.tab] = selected;
                                        editor.activate();
                                        view.discard_armed = false;
                                    }
                                }
                                view.pane(Pane::SettingsDetails).offset = 0;
                                continue;
                            }
                            view.selection = view.selectable.iter()
                                .find(|area| area.contains((mouse.column, mouse.row).into()))
                                .map(|area| text_input::Selection {
                                    area: *area, start: (mouse.column, mouse.row), end: (mouse.column, mouse.row),
                                });
                        }
                        MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                            if let Some(selection) = &mut view.selection {
                                selection.extend(mouse.column, mouse.row);
                            }
                        }
                        MouseEventKind::Down(MouseButton::Right) if !view.copy_selection().await => {
                            view.paste_clipboard().await;
                        }
                        _ => {}
                    }
                    continue;
                }
                let Event::Key(key) = event else { continue; };
                if key.kind == KeyEventKind::Release { continue; }
                if key.kind == KeyEventKind::Repeat && key.code == KeyCode::Enter { continue; }
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) && view.copy_selection().await {
                    continue;
                }
                if (ctrl && matches!(key.code, KeyCode::Char('v' | 'V')))
                    || (key.modifiers.contains(KeyModifiers::SHIFT) && key.code == KeyCode::Insert) {
                    if key.kind != KeyEventKind::Repeat { view.paste_clipboard().await; }
                    continue;
                }
                // Reveal live requests before allowing keys to act on a frame
                // that was frozen for copying.
                let saving_settings = matches!(view.screen, Screen::Settings)
                    && ((ctrl && matches!(key.code, KeyCode::Char('s' | 'S'))) || key.code == KeyCode::F(10));
                if view.selection.take().is_some() && !saving_settings { continue; }
                if key.code == KeyCode::F(6) {
                    view.quit_armed = false;
                    view.native_selection = !view.native_selection;
                    if view.native_selection {
                        crossterm::execute!(std::io::stdout(), DisableMouseCapture)?;
                    } else {
                        crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
                    }
                    continue;
                }
                if ctrl && key.code == KeyCode::Char('q') { break; }
                if ctrl && key.code == KeyCode::Char('c') {
                    if key.kind == KeyEventKind::Repeat { continue; }
                    if view.quit_armed { break; }
                    view.quit_armed = true;
                } else {
                    view.quit_armed = false;
                }
                if key.code == KeyCode::F(2) {
                    view.screen = Screen::Permissions; view.pane(Pane::Permissions).offset = 0; continue;
                }
                if key.code == KeyCode::F(3) {
                    view.screen = Screen::Agents; view.pane(Pane::AgentDetails).offset = 0; continue;
                }
                if key.code == KeyCode::F(4) { view.open_appearance(); continue; }
                if key.code == KeyCode::F(5) {
                    if !matches!(view.screen, Screen::Settings) { view.open_settings(session.as_ref()).await; }
                    continue;
                }
                if matches!(view.screen, Screen::Permissions) && key.code == KeyCode::Char('e') {
                    view.open_settings(session.as_ref()).await;
                    continue;
                }
                if matches!(view.screen, Screen::Settings) && !(ctrl && key.code == KeyCode::Char('c')) {
                    if (ctrl && matches!(key.code, KeyCode::Char('s' | 'S'))) || key.code == KeyCode::F(10) {
                        view.save_settings(session.as_ref(), key.code == KeyCode::F(10)).await;
                    } else if key.code == KeyCode::Esc {
                        view.close_settings();
                    } else if let Some(editor) = &mut view.settings {
                        view.discard_armed = false;
                        match key.code {
                            KeyCode::Tab | KeyCode::Right => editor.tab = (editor.tab + 1) % 4,
                            KeyCode::BackTab | KeyCode::Left => editor.tab = (editor.tab + 3) % 4,
                            KeyCode::Up | KeyCode::Down => editor.move_selection(key.code == KeyCode::Up),
                            KeyCode::Char(' ') | KeyCode::Enter => editor.activate(),
                            KeyCode::Char('p') => editor.save_project = !editor.save_project,
                            KeyCode::Char('a') => editor.cycle_tool_permission(),
                            KeyCode::PageUp | KeyCode::PageDown => {
                                let state = view.pane(Pane::SettingsDetails);
                                state.scroll(key.code == KeyCode::PageUp, state.page.saturating_sub(1).max(1));
                                continue;
                            }
                            _ => {}
                        }
                    }
                    if let Some(editor) = &view.settings {
                        let selected = editor.selected[editor.tab];
                        let state = view.pane(Pane::SettingsList);
                        if selected < state.offset { state.offset = selected; }
                        else if selected >= state.offset + state.page { state.offset = (selected + 1).saturating_sub(state.page); }
                    }
                    view.pane(Pane::SettingsDetails).offset = 0;
                    continue;
                }
                if !matches!(view.screen, Screen::Conversation) && !(ctrl && key.code == KeyCode::Char('c')) {
                    match key.code {
                        KeyCode::Esc => { view.screen = Screen::Conversation; },
                        KeyCode::Enter if matches!(view.screen, Screen::Permissions) => {
                            view.permission_selected = 1;
                            view.screen = Screen::PermissionConsent;
                        },
                        KeyCode::Up | KeyCode::Down if matches!(view.screen, Screen::PermissionConsent) => {
                            view.permission_selected = 1 - view.permission_selected;
                        },
                        KeyCode::Enter if matches!(view.screen, Screen::PermissionConsent) => {
                            if view.permission_selected == 0 {
                                if let Some(session) = &session {
                                    match session.enable_tool_approvals(&id("permissions")).await {
                                        Ok(_) => {
                                            view.refresh_permissions(&session.snapshot().await?);
                                            view.append("\nPermissions updated: all tools can request explicit approval. No action has been executed.\n");
                                            view.error.clear();
                                            view.screen = Screen::Conversation;
                                        },
                                        Err(error) => view.error = error.to_string(),
                                    }
                                }
                            } else {
                                view.screen = Screen::Conversation;
                            }
                        },
                        KeyCode::Up | KeyCode::Down if matches!(view.screen, Screen::Appearance) => {
                            view.layout_selected = 1 - view.layout_selected;
                        },
                        KeyCode::Enter if matches!(view.screen, Screen::Appearance) => {
                            let layout = if view.layout_selected == 0 { LayoutMode::Compact } else { LayoutMode::Cockpit };
                            match save_layout(layout).await {
                                Ok(()) => { view.layout = layout; view.screen = Screen::Conversation; view.error.clear(); },
                                Err(error) => view.error = format!("Could not save layout ({}).", error.code),
                            }
                        },
                        KeyCode::Up | KeyCode::Down if matches!(view.screen, Screen::Agents) && !view.agent_views.is_empty() => {
                            let count = view.agent_views.len();
                            view.agent_selected = if key.code == KeyCode::Up { (view.agent_selected + count - 1) % count } else { (view.agent_selected + 1) % count };
                            view.pane(Pane::AgentDetails).offset = 0;
                            let selected = view.agent_selected;
                            let list = view.pane(Pane::AgentList);
                            if selected < list.offset { list.offset = selected; }
                            else if selected >= list.offset + list.page { list.offset = (selected + 1).saturating_sub(list.page); }
                        },
                        KeyCode::PageUp | KeyCode::PageDown => {
                            let pane = if matches!(view.screen, Screen::Agents) { Pane::AgentDetails } else { Pane::Permissions };
                            let state = view.pane(pane);
                            state.scroll(key.code == KeyCode::PageUp, state.page.saturating_sub(1).max(1));
                        },
                        _ => {}
                    }
                    continue;
                }
                let commands = view.commands();
                if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) && !commands.is_empty() {
                    match key.code {
                        KeyCode::Up => { view.command_selected = (view.command_selected + commands.len() - 1) % commands.len(); continue; },
                        KeyCode::Down => { view.command_selected = (view.command_selected + 1) % commands.len(); continue; },
                        KeyCode::Tab => { view.input = commands[view.command_selected.min(commands.len() - 1)].0.into(); view.command_selected = 0; continue; },
                        KeyCode::Esc => { view.commands_dismissed = true; continue; },
                        KeyCode::Enter => { view.input = commands[view.command_selected.min(commands.len() - 1)].0.into(); },
                        _ => {}
                    }
                }
                if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) && view.input.is_empty()
                    && matches!(key.code, KeyCode::Up | KeyCode::Down)
                {
                    let count = view.response_options().len();
                    if count > 0 {
                        view.choice_selected = if key.code == KeyCode::Up { (view.choice_selected + count - 1) % count } else { (view.choice_selected + 1) % count };
                        continue;
                    }
                }
                if key.code == KeyCode::Enter && view.input == "/quit" { break; }
                if key.code == KeyCode::Enter && matches!(view.input.as_str(), "/setup" | "/sessions") {
                    if let Some(run)=&active && !run.status().await?.status.terminal() {
                        view.error="Cancel or finish the current run before changing the connection or session.".into();
                        continue;
                    }
                    exit = if view.input == "/setup" { Exit::Setup } else { Exit::Sessions }; break;
                }
                let result: Result<()> = async {
                    if ctrl && key.code == KeyCode::Char('c') {
                        if let Some(run) = &active && !run.status().await?.status.terminal() { run.cancel("TUI Ctrl-C", &id("cancel")).await?; }
                    } else if (ctrl && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('n')))
                        || (key.code == KeyCode::Enter && view.input.is_empty() && view.approval.is_some()) {
                        if let Some(run) = addressed.as_ref().or(active.as_ref()) {
                            let choice = if ctrl { usize::from(key.code != KeyCode::Char('y')) } else { view.choice_selected };
                            approve_choice(&mut view, run, choice, session.as_ref()).await?;
                        }
                    } else if ctrl && key.code == KeyCode::Char('u') {
                        view.input.clear(); view.edited_input();
                    } else if key.code == KeyCode::Enter && view.input.is_empty() && view.interaction.is_some() {
                        let answer = view.interaction.as_ref().and_then(|q| q.choices.get(view.choice_selected)).cloned();
                        if let (Some(run), Some(answer)) = (addressed.as_ref().or(active.as_ref()), answer) {
                            answer_question(&mut view, run, &answer).await?;
                        } else if view.choice_selected + 1 == view.response_options().len() {
                            view.open_settings(session.as_ref()).await;
                            if let Some(editor) = &mut view.settings {
                                editor.tab = 1;
                                editor.selected[1] = 1;
                            }
                        }
                    } else if key.modifiers.contains(KeyModifiers::ALT) && matches!(key.code, KeyCode::Up | KeyCode::Down) {
                        let pane = if view.approval.is_some() || view.interaction.is_some() { Pane::Request } else { Pane::Session };
                        view.pane(pane).scroll(key.code == KeyCode::Up, 3);
                    } else { match key.code {
                        KeyCode::PageUp => view.scroll_chat(true, view.chat_page.saturating_sub(1).max(1)),
                        KeyCode::PageDown => view.scroll_chat(false, view.chat_page.saturating_sub(1).max(1)),
                        KeyCode::Home => view.scroll = Some(0),
                        KeyCode::End => view.scroll = None,
                        KeyCode::Backspace => view.backspace_input(),
                        KeyCode::Char(c) if !ctrl && !c.is_control() && view.expanded_input().len() + c.len_utf8() <= text_input::INPUT_LIMIT => { view.input.push(c); view.edited_input(); },
                        KeyCode::Enter if !view.input.trim().is_empty() => {
                            let input = view.input.clone();
                            if input.starts_with("/permissions ") {
                                let session = session.as_ref().ok_or_else(|| WorkerError::new("session_required", "Connect a session first"))?;
                                let message = crate::cli_config::permission_command(session, &input).await?;
                                view.append(&format!("\n\nPermissions · {message}\n"));
                                view.refresh_permissions(&session.snapshot().await?);
                                view.input.clear(); view.edited_input();
                                return Ok(());
                            }
                            if COMMANDS.iter().any(|(name, _)| *name == input) || (view.interaction.is_none() && input.starts_with('/')) {
                                match input.as_str() {
                                    "/help" => {
                                        view.append("\n\nCommands:\n");
                                        for (name, description) in COMMANDS { view.append(&format!("{name}: {description}\n")); }
                                        view.append(&format!("{}\n", crate::cli_config::COMMAND_HELP));
                                        view.append("↑/↓ select · Tab complete · Enter run · Esc close\nF2 permissions · F3 subagents · F4 Compact/Cockpit · F5 settings\nSettings: Tab sections · Space/Enter toggle · Ctrl-S save session/project defaults\nQuestion: ↑/↓ + Enter, or type an answer\nDrag: select text · Ctrl-C: copy selection · Esc: clear selection\nCtrl-V / Shift-Insert: paste · Right-click: copy selection or paste\nF6: toggle native terminal selection · Ctrl-Shift-V: terminal paste\nWithout selection: Ctrl-C cancels; twice quits · Ctrl-Q quits\nCtrl-Y allows once · Ctrl-N denies · Wheel scrolls the pane under the pointer\nPgUp/PgDn scroll chat · Home: oldest · End: latest and follow output\nAlt-Up/Down scroll request details or session sidebar · Ctrl-U clears input\n");
                                    },
                                    "/clear" => view.transcript.clear(),
                                    "/agents" => { view.screen = Screen::Agents; view.pane(Pane::AgentDetails).offset = 0; },
                                    "/permissions" => { view.screen = Screen::Permissions; view.pane(Pane::Permissions).offset = 0; },
                                    "/view" => view.open_appearance(),
                                    "/settings" => view.open_settings(session.as_ref()).await,
                                    "/cancel" => { if let Some(run) = &active && !run.status().await?.status.terminal() { run.cancel("TUI /cancel", &id("cancel")).await?; } },
                                    _ => return Err(WorkerError::new("unknown_command", "Unknown command. Type / for suggestions or /help.")),
                                }
                                view.input.clear(); view.edited_input(); view.scroll = None;
                                return Ok(());
                            }
                            let display_input = input;
                            let input = view.expanded_input();
                            if input.len() > text_input::INPUT_LIMIT {
                                return Err(WorkerError::new("input_limit", "Expanded message exceeds 64 KiB. Remove a pasted block before sending."));
                            }
                            if view.approval.is_some() {
                                let allow = approval_answer(&input).ok_or_else(|| WorkerError::new("approval_pending", "Type ja/yes or nein/no, choose Allow once/Deny, or use Ctrl-Y/Ctrl-N."))?;
                                if let Some(run) = addressed.as_ref().or(active.as_ref()) {
                                    approve_action(&mut view, run, allow).await?;
                                }
                                return Ok(());
                            }
                            if let Some(run) = addressed.as_ref().or(active.as_ref()) && view.interaction.is_some() {
                                answer_question(&mut view, run, &input).await?;
                                return Ok(());
                            } else if let Some(session) = &session {
                                if let Some(run) = &active && run.status().await?.status == RunStatus::Interrupted {
                                    return Err(WorkerError::new("resume_required", RESUME_REQUIRED));
                                }
                                if let Some(run) = &active && !run.status().await?.status.terminal() {
                                    run.steer(&input, &id("steer")).await?;
                                } else {
                                    active = Some(session.submit(&input, &id("tui"), limits.clone(), None).await?);
                                }
                            } else { return Err(WorkerError::new("provider_required", "Connect Werk or start ww tui --demo")); }
                            view.append(&format!("\n\nyou › {display_input}\n")); view.input.clear(); view.edited_input(); view.scroll = None;
                        },
                        _ => {}
                    }}
                    Ok(())
                }.await;
                view.error = result.err().map(|e| e.to_string()).unwrap_or_default();
            }
        }
    }
    if let Some(run) = active
        && !run.status().await?.status.terminal()
    {
        run.cancel("TUI closed", &id("cancel")).await?;
        run.wait().await?;
    }
    Ok(exit)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paste_stays_in_the_draft_and_does_not_answer_hidden_requests() {
        let mut view = View::default();
        view.paste("ja\r\n/quit\r\n");
        assert_eq!(view.input, "ja\n/quit\n");
        assert!(view.transcript.is_empty());
        view.screen = Screen::Permissions;
        view.paste("hidden answer");
        assert_eq!(view.input, "ja\n/quit\n");
    }

    #[test]
    fn large_pastes_are_lossless_numbered_atomic_draft_blocks() {
        let mut view = View {
            input: "Explain: ".into(),
            ..Default::default()
        };
        let first = (0..24)
            .map(|n| format!("Zeile {n} 世界"))
            .collect::<Vec<_>>()
            .join("\n");
        view.paste(&first);
        assert_eq!(view.input, "Explain: [Pasted text #1 +24 lines]");
        view.input.push_str(" then ");
        let second = format!("{}\n[Pasted text #1 +24 lines]", "x".repeat(20_000));
        view.paste(&second);
        assert_eq!(
            view.expanded_input(),
            format!("Explain: {first} then {second}")
        );
        assert!(view.input.ends_with("[Pasted text #2 +2 lines]"));
        view.backspace_input();
        assert_eq!(view.expanded_input(), format!("Explain: {first} then "));
        assert_eq!(view.pasted_texts.len(), 1);
        let before = view.input.clone();
        view.paste(&"x".repeat(text_input::INPUT_LIMIT));
        assert_eq!(view.input, before);
        assert!(view.error.contains("nothing was added"));
        view.input.clear();
        view.edited_input();
        assert!(view.pasted_texts.is_empty());
        view.paste(&first);
        assert_eq!(view.input, "[Pasted text #3 +24 lines]");
    }

    #[test]
    fn wheel_scrolls_each_pane_independently_and_clamps_to_content() {
        for layout in [LayoutMode::Compact, LayoutMode::Cockpit] {
            let mut view = View {
                layout,
                transcript: "chat row\n".repeat(100),
                capacity: "session row\n".repeat(100),
                permissions: "permission row\n".repeat(100),
                input: "input row\n".repeat(20),
                interaction: Some(InteractionRequest {
                    request_id: "q".into(),
                    question: "question row\n".repeat(100),
                    choices: vec![],
                    answer: None,
                }),
                agent_views: (0..30)
                    .map(|n| AgentView {
                        run_id: n.to_string(),
                        label: format!("child {n}"),
                        details: "agent details\n".repeat(100),
                    })
                    .collect(),
                ..Default::default()
            };
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
            for (screen, panes) in [
                (
                    Screen::Conversation,
                    vec![Pane::Request, Pane::Input, Pane::Session],
                ),
                (Screen::Agents, vec![Pane::AgentList, Pane::AgentDetails]),
                (Screen::Permissions, vec![Pane::Permissions]),
            ] {
                view.screen = screen;
                terminal.draw(|f| draw(f, &mut view)).unwrap();
                for pane in panes {
                    let state = *view.pane(pane);
                    if state.area.is_empty() {
                        continue;
                    }
                    assert!(state.max > 3);
                    let before = view.panes.map(|p| p.offset);
                    let chat_before = view.scroll;
                    view.scroll_at(state.area.x, state.area.y, false);
                    terminal.draw(|f| draw(f, &mut view)).unwrap();
                    assert_eq!(view.pane(pane).offset, 3);
                    assert_eq!(view.scroll, chat_before);
                    for (i, previous) in before.iter().enumerate() {
                        if i != pane as usize {
                            assert_eq!(view.panes[i].offset, *previous);
                        }
                    }
                    view.pane(pane).scroll(false, usize::MAX);
                    terminal.draw(|f| draw(f, &mut view)).unwrap();
                    let state = view.pane(pane);
                    assert_eq!(state.offset, state.max);
                    view.pane(pane).scroll(true, usize::MAX);
                    assert_eq!(view.pane(pane).offset, 0);
                }
            }
            // Invisible pane hit boxes cannot intercept another screen's wheel.
            assert!(view.pane(Pane::Request).area.is_empty());
            assert!(view.pane(Pane::Session).area.is_empty());
        }
    }

    #[test]
    fn selected_frame_stays_stable_during_output_and_is_cleared_on_resize() {
        let mut view = View {
            transcript: "selected text".into(),
            ..Default::default()
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut view)).unwrap();
        let area = view.selectable[0];
        view.selection = Some(text_input::Selection {
            area,
            start: (area.x, area.y),
            end: (area.x + 12, area.y),
        });
        view.append(&"\nnew output".repeat(30));
        terminal.draw(|f| draw(f, &mut view)).unwrap();
        assert_eq!(
            view.selection
                .as_ref()
                .unwrap()
                .text(view.rendered.as_ref().unwrap()),
            "selected text"
        );
        terminal.backend_mut().resize(30, 12);
        terminal.draw(|f| draw(f, &mut view)).unwrap();
        assert!(view.selection.is_none());
    }

    #[test]
    fn scrolling_uses_rendered_rows_and_holds_position_during_streaming() {
        let mut view = View::default();
        for n in 0..80 {
            view.append(&format!(
                "line-{n:03} 世界 some words that wrap at narrow widths\n"
            ));
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 18)).unwrap();
        terminal.draw(|f| draw(f, &mut view)).unwrap();
        assert!(view.chat_max_scroll > 80);
        let bottom = view.chat_max_scroll;
        view.scroll_chat(true, 12);
        terminal.draw(|f| draw(f, &mut view)).unwrap();
        let position = view.scroll.unwrap();
        assert_eq!(position, bottom - 12);
        let first_row = |t: &ratatui::Terminal<ratatui::backend::TestBackend>, v: &View| {
            let buffer = t.backend().buffer();
            (v.chat_area.x + 1..v.chat_area.right() - 1)
                .map(|x| buffer[(x, v.chat_area.y + 1)].symbol())
                .collect::<String>()
        };
        let before = first_row(&terminal, &view);
        view.append(&"new output\n".repeat(20));
        terminal.draw(|f| draw(f, &mut view)).unwrap();
        assert_eq!(view.scroll, Some(position));
        assert_eq!(first_row(&terminal, &view), before);
        view.scroll_chat(false, usize::MAX);
        terminal.draw(|f| draw(f, &mut view)).unwrap();
        assert_eq!(view.scroll, None);
        assert!(view.chat_max_scroll > bottom);
    }

    #[test]
    fn fenced_code_has_a_muted_background_and_preserves_indentation() {
        let mut view = View::default();
        view.append("werk › ```rust\nfn main() {\n    let message = \"Grüße 世界\"; // explanation\n}\n```\nProse after code.");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut view)).unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("│     let message"));
        assert!(text.contains("Prose after code."));
        assert!(!text.contains("```"));
        let cells: Vec<_> = buffer
            .content()
            .iter()
            .filter(|c| c.bg == crate::cli_transcript::CODE_BG)
            .collect();
        assert!(!cells.is_empty());
        assert!(cells.iter().any(|c| c.fg == Color::Rgb(183, 157, 174)));
        assert!(cells.iter().any(|c| c.fg == Color::Rgb(169, 185, 141)));
        assert!(cells.iter().any(|c| c.fg == Color::Rgb(139, 148, 129)));
    }

    #[test]
    fn command_preview_filters_and_preserves_question_answers() {
        let mut view = View {
            input: "/".into(),
            ..Default::default()
        };
        assert_eq!(view.commands().len(), COMMANDS.len());
        view.input = "/ca".into();
        assert_eq!(view.commands()[0].0, "/cancel");
        view.commands_dismissed = true;
        assert!(view.commands().is_empty());
        view.edited_input();
        assert_eq!(view.commands().len(), 1);
        view.interaction = Some(InteractionRequest {
            request_id: "question".into(),
            question: "Path?".into(),
            choices: vec![],
            answer: None,
        });
        assert_eq!(view.commands()[0].0, "/cancel");
        assert_eq!(view.input, "/ca");
        view.input = "/unknown".into();
        assert!(view.commands().is_empty());
        assert_eq!(view.input, "/unknown");
        view.interaction = None;
        view.input = "/".into();
        for size in [(100, 30), (20, 8), (1, 1)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(size.0, size.1)).unwrap();
            terminal.draw(|f| draw(f, &mut view)).unwrap();
        }
    }
    #[test]
    fn palette_layout_and_untrusted_text_are_safe() {
        let mut view = View {
            status: "Ready".into(),
            ..Default::default()
        };
        view.append("hello\x1b]52;c;payload\x07世界");
        assert!(!view.transcript.contains('\x1b'));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut view)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].bg, BG);
        assert_eq!(buffer[(3, 0)].fg, BRAND[0]);
        let art = banner()
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(art.contains("werkWorker · Agent Engine"));
        assert!(art.contains(r"\ \      / /"));
        assert!(art.contains("/ //_/"));
        assert!(art.lines().all(|line| line.len() <= 94));
        view.append(&"x".repeat(200_000));
        assert!(view.transcript.len() <= 131_072);
        for size in [(20, 8), (1, 1)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(size.0, size.1)).unwrap();
            terminal.draw(|f| draw(f, &mut view)).unwrap();
        }
    }

    #[test]
    fn inspectors_and_full_width_requests_render_on_small_terminals() {
        let mut view = View {
            agent_views: vec![AgentView {
                run_id: "child".into(),
                label: "WaitingForApproval · child".into(),
                details: "Tool: files.write\nPending approval".into(),
            }],
            permissions: "Profile: general-readonly\nWorkspace writes: disabled".into(),
            interaction: Some(InteractionRequest {
                request_id: "q".into(),
                question: "Can I write?".into(),
                choices: vec!["Yes".into()],
                answer: None,
            }),
            ..Default::default()
        };
        for screen in [
            Screen::Conversation,
            Screen::Agents,
            Screen::Permissions,
            Screen::PermissionConsent,
            Screen::Appearance,
        ] {
            view.screen = screen;
            for (width, height) in [(130, 36), (40, 12), (1, 1)] {
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                terminal.draw(|f| draw(f, &mut view)).unwrap();
                if width == 130 {
                    let text = terminal
                        .backend()
                        .buffer()
                        .content()
                        .iter()
                        .map(|c| c.symbol())
                        .collect::<String>();
                    assert!(text.contains(match view.screen {
                        Screen::Conversation => "Can I write?",
                        Screen::Agents => "Pending approval",
                        Screen::Permissions => "Workspace writes: disabled",
                        Screen::PermissionConsent => "Enable tool approval requests",
                        Screen::Appearance => "Choose your layout",
                        Screen::Settings => "Settings",
                    }));
                }
            }
        }
    }
}
