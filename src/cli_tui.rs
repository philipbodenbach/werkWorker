//! Interactive CLI client. Engine policy and lifecycle stay in the public SDK.
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::{Stream, StreamExt};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};
use std::{io::IsTerminal, pin::Pin, time::Duration};
use werk_worker::*;

// Exact werk1112 top/terminal palette; attribution in NOTICE.
pub(crate) const BG: Color = Color::Rgb(9, 13, 23);
const PANEL: Color = Color::Rgb(15, 21, 35);
pub(crate) const TEXT: Color = Color::Rgb(224, 230, 248);
pub(crate) const MUTED: Color = Color::Rgb(129, 143, 174);
pub(crate) const BRAND: [Color; 5] = [
    Color::Rgb(0, 220, 255),
    Color::Rgb(59, 130, 246),
    Color::Rgb(99, 102, 241),
    Color::Rgb(139, 92, 246),
    Color::Rgb(255, 79, 195),
];

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
const COMMANDS: [(&str, &str); 9] = [
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
    Appearance,
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
#[derive(Default)]
struct View {
    transcript: String,
    input: String,
    status: String,
    session_id: String,
    run_id: String,
    approval: Option<ApprovalRequest>,
    interaction: Option<InteractionRequest>,
    scroll: u16,
    detail_scroll: u16,
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
    agent_scroll: u16,
    permissions: String,
    access_summary: String,
    layout: LayoutMode,
    layout_selected: usize,
    choice_selected: usize,
}
impl View {
    fn response_options(&self) -> Vec<String> {
        if self.approval.is_some() {
            vec!["Allow once".into(), "Deny".into()]
        } else if let Some(question) = &self.interaction {
            let mut options = question.choices.clone();
            if !options.is_empty() {
                options.push("Write my own answer".into());
            }
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
        if data.policy.workspace_writable {
            "enabled (within grants)"
        } else {
            "disabled"
        },
        if data.profile.tools.iter().any(|t| t == "process.run") {
            if data.policy.grants.iter().any(|g| g.tool == "process.run") {
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
    let details = format!(
        "{summary}\nWorkspace: {}\n\nAvailable profile tools:\n{}\n\nHost grants:\n{}\n\nHard-denied tools: {}\n\nChange access:\n1. /cancel the current run (including subagents), if running.\n2. /setup opens a new session.\n3. For editing and cargo test: choose coding-basic and\n   Workspace access: Read/write + sandbox processes.\n4. Submit the task again.\n\nAn answer to an agent question does not grant permissions.\nCtrl-Y approves only the displayed prepared action within existing grants.\nSubagents inherit these limits; they cannot add tools or rights.",
        data.workspace.display(),
        data.profile.tools.join(", "),
        if grants.is_empty() { "None" } else { &grants },
        if data.policy.hard_denied_tools.is_empty() {
            "None".into()
        } else {
            data.policy.hard_denied_tools.join(", ")
        },
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
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(PANEL).fg(TEXT))
}
fn wrapped_lines(text: &str, width: u16) -> usize {
    let width = usize::from(width.max(1));
    text.lines()
        .map(|line| line.chars().count().max(1).div_ceil(width))
        .sum()
}

fn draw_request(frame: &mut Frame, view: &View, area: Rect) {
    let (title, body) = if let Some(request) = &view.approval {
        (
            " Approval ",
            format!(
                "Agent: {}\nTool: {}\n{}\n\nAllows only this prepared action; existing permissions still apply.",
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
    frame.render_widget(
        Paragraph::new(clean(&body))
            .wrap(Wrap { trim: false })
            .scroll((view.detail_scroll, 0)),
        rows[0],
    );
    let selected = view.choice_selected.min(options.len().saturating_sub(1));
    let first = (selected + 1).saturating_sub(option_height as usize);
    let choices: Vec<Line<'_>> = options
        .iter()
        .enumerate()
        .skip(first)
        .map(|(i, option)| {
            Line::styled(
                format!(
                    "{} {}",
                    if i == selected && view.input.is_empty() {
                        "›"
                    } else {
                        " "
                    },
                    clean(option)
                ),
                if i == selected && view.input.is_empty() {
                    Style::default().fg(BG).bg(BRAND[4]).bold()
                } else {
                    Style::default().fg(TEXT)
                },
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
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(BRAND[4])),
        rows[2],
    );
}

fn draw_chat(frame: &mut Frame, view: &View, area: Rect) {
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
    let offset = wrapped_lines(&view.transcript, chat.width.saturating_sub(2))
        .saturating_sub(height)
        .min(u16::MAX as usize) as u16;
    frame.render_widget(
        Paragraph::new(view.transcript.as_str())
            .wrap(Wrap { trim: false })
            .scroll((offset.saturating_sub(view.scroll), 0))
            .block(panel(
                " Conversation ",
                if view.layout == LayoutMode::Compact {
                    MUTED
                } else {
                    BRAND[0]
                },
            )),
        chat,
    );
    if let Some(area) = request {
        draw_request(frame, view, area);
    }
}

fn draw_agents(frame: &mut Frame, view: &View, area: Rect) {
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
                Line::styled(
                    format!(
                        "{} {}",
                        if i == view.agent_selected { ">" } else { " " },
                        agent.label
                    ),
                    if i == view.agent_selected {
                        Style::default().fg(BG).bg(BRAND[4])
                    } else {
                        Style::default().fg(TEXT)
                    },
                )
            })
            .collect()
    };
    let list_scroll =
        (view.agent_selected + 1).saturating_sub(split[0].height.saturating_sub(2) as usize);
    frame.render_widget(
        Paragraph::new(items)
            .scroll((list_scroll.min(u16::MAX as usize) as u16, 0))
            .block(panel(" Subagents · Up/Down select ", BRAND[4])),
        split[0],
    );
    let details = view
        .agent_views
        .get(view.agent_selected)
        .map(|a| a.details.as_str())
        .unwrap_or(
            "Subagent progress and tool results appear here.\nEsc returns to the conversation.",
        );
    frame.render_widget(
        Paragraph::new(details)
            .wrap(Wrap { trim: false })
            .scroll((view.agent_scroll, 0))
            .block(panel(
                " Agent details · PgUp/PgDn scroll · Esc close ",
                BRAND[3],
            )),
        split[1],
    );
}

fn draw(frame: &mut Frame, view: &View) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(BG).fg(TEXT)),
        area,
    );
    let wide_banner = view.layout == LayoutMode::Cockpit && area.width >= 94 && area.height >= 24;
    let rows = Layout::vertical([
        Constraint::Length(if wide_banner { 8 } else { 2 }),
        Constraint::Min(1),
        Constraint::Length(3),
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
                    "Status: {}\n{}\n{}\n\n{}\n{}\n\n{}\n\n{}\n\nModel requests: {}\nTool calls: {}\n\nF2 permissions · F3 agents\nF4 view menu",
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
                frame.render_widget(
                    Paragraph::new(clean(&details))
                        .wrap(Wrap { trim: false })
                        .block(panel(" Session ", BRAND[3])),
                    columns[1],
                );
            } else {
                draw_chat(frame, view, rows[1]);
            }
        }
        Screen::Agents => draw_agents(frame, view, rows[1]),
        Screen::Permissions => {
            frame.render_widget(
                Paragraph::new(clean(&view.permissions))
                    .wrap(Wrap { trim: false })
                    .scroll((view.detail_scroll, 0))
                    .block(panel(
                        " Permissions · PgUp/PgDn scroll · Esc close ",
                        BRAND[3],
                    )),
                rows[1],
            );
        }
        Screen::Appearance => {
            let labels = [
                "Compact — one chat column, inline questions and answers",
                "Cockpit — conversation with a session sidebar",
            ];
            let mut lines = vec![Line::from("Choose your layout"), Line::from("")];
            for (i, label) in labels.iter().enumerate() {
                lines.push(Line::styled(
                    format!(
                        "{} {label}",
                        if i == view.layout_selected {
                            "›"
                        } else {
                            " "
                        }
                    ),
                    if i == view.layout_selected {
                        Style::default().fg(BG).bg(BRAND[4]).bold()
                    } else {
                        Style::default().fg(TEXT)
                    },
                ));
                lines.push(Line::from(""));
            }
            lines.push(Line::from(
                "Up/Down: select · Enter: apply and remember · Esc: cancel",
            ));
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
        " Approval · ↑↓ choose · Enter confirm "
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
    let tail: String = view
        .input
        .chars()
        .rev()
        .take(usize::from(rows[2].width.saturating_sub(4)))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    frame.render_widget(
        Paragraph::new(format!("› {tail}")).block(panel(
            input_title,
            if pending { BRAND[4] } else { BRAND[1] },
        )),
        rows[2],
    );
    let footer = if view.quit_armed {
        " Press Ctrl-C again to quit. Any other key continues."
    } else if !view.error.is_empty() {
        &view.error
    } else {
        " F2 permissions · F3 agents · F4 view · / commands · Ctrl-C cancel · Ctrl-Q quit"
    };
    frame.render_widget(
        Paragraph::new(clean(footer)).style(Style::default().fg(if view.error.is_empty() {
            MUTED
        } else {
            Color::Rgb(255, 90, 110)
        })),
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
                    Line::styled(
                        format!(
                            "{} {name:<13} {description}",
                            if i == selected { "›" } else { " " }
                        ),
                        if i == selected {
                            Style::default().fg(BG).bg(BRAND[4]).bold()
                        } else {
                            Style::default().fg(TEXT)
                        },
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
        }
    }
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
        view.detail_scroll = 0;
        view.choice_selected = usize::from(approval.is_some());
        if interaction.is_some() {
            view.input.clear();
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
        view.append(&format!(
            "\n\n{} asks > {}\nyou › {answer}\n",
            view.request_owner, request.question
        ));
        view.interaction = None;
        view.input.clear();
        view.edited_input();
        view.scroll = 0;
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
        (view.access_summary, view.permissions) = permission_details(&data);
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
            _ = tick.tick() => { if dirty { terminal.draw(|frame| draw(frame, &view))?; dirty = false; } },
            event = events.next() => match event {
                Some(Ok(event)) => { view.event(event); dirty = true; },
                Some(Err(error)) => return Err(error),
                None => break,
            },
            key = keys.next() => {
                let Some(key) = key else { break; };
                dirty = true;
                let Event::Key(key) = key? else { continue; };
                if key.kind == KeyEventKind::Release { continue; }
                if key.kind == KeyEventKind::Repeat && key.code == KeyCode::Enter { continue; }
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                if ctrl && key.code == KeyCode::Char('q') { break; }
                if ctrl && key.code == KeyCode::Char('c') {
                    if key.kind == KeyEventKind::Repeat { continue; }
                    if view.quit_armed { break; }
                    view.quit_armed = true;
                } else {
                    view.quit_armed = false;
                }
                if key.code == KeyCode::F(2) {
                    view.screen = Screen::Permissions; view.detail_scroll = 0; continue;
                }
                if key.code == KeyCode::F(3) {
                    view.screen = Screen::Agents; view.agent_scroll = 0; continue;
                }
                if key.code == KeyCode::F(4) { view.open_appearance(); continue; }
                if !matches!(view.screen, Screen::Conversation) && !(ctrl && key.code == KeyCode::Char('c')) {
                    match key.code {
                        KeyCode::Esc => { view.screen = Screen::Conversation; view.detail_scroll = 0; },
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
                            view.agent_scroll = 0;
                        },
                        KeyCode::PageUp | KeyCode::PageDown => {
                            let scroll = if matches!(view.screen, Screen::Agents) { &mut view.agent_scroll } else { &mut view.detail_scroll };
                            *scroll = if key.code == KeyCode::PageUp { scroll.saturating_sub(10) } else { scroll.saturating_add(10) };
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
                        if let (Some(run), Some(request)) = (addressed.as_ref().or(active.as_ref()), &view.approval) {
                            let allow = if ctrl { key.code == KeyCode::Char('y') } else { view.choice_selected == 0 };
                            run.respond_to_approval(&request.request_id, if allow { ApprovalDecision::AllowOnce } else { ApprovalDecision::Deny }, &id("approval")).await?;
                            let tool = request.description["tool"].as_str().unwrap_or("action").to_owned();
                            view.append(&format!("\n\n{} · {tool}: {}\n", view.request_owner, if allow { "Allowed once" } else { "Denied" }));
                            view.approval = None;
                        }
                    } else if ctrl && key.code == KeyCode::Char('u') {
                        view.input.clear(); view.edited_input();
                    } else if key.code == KeyCode::Enter && view.input.is_empty() && view.interaction.is_some() {
                        let answer = view.interaction.as_ref().and_then(|q| q.choices.get(view.choice_selected)).cloned();
                        if let (Some(run), Some(answer)) = (addressed.as_ref().or(active.as_ref()), answer) {
                            answer_question(&mut view, run, &answer).await?;
                        }
                    } else if key.modifiers.contains(KeyModifiers::ALT) && matches!(key.code, KeyCode::Up | KeyCode::Down) {
                        view.detail_scroll = if key.code == KeyCode::Up { view.detail_scroll.saturating_sub(3) } else { view.detail_scroll.saturating_add(3) };
                    } else { match key.code {
                        KeyCode::PageUp => view.scroll = view.scroll.saturating_add(10),
                        KeyCode::PageDown => view.scroll = view.scroll.saturating_sub(10),
                        KeyCode::Backspace => { view.input.pop(); view.edited_input(); },
                        KeyCode::Char(c) if !ctrl && !c.is_control() && view.input.len() < 16_384 => { view.input.push(c); view.edited_input(); },
                        KeyCode::Enter if !view.input.trim().is_empty() => {
                            let input = view.input.clone();
                            if COMMANDS.iter().any(|(name, _)| *name == input) || (view.interaction.is_none() && input.starts_with('/')) {
                                match input.as_str() {
                                    "/help" => {
                                        view.append("\n\nCommands:\n");
                                        for (name, description) in COMMANDS { view.append(&format!("{name}: {description}\n")); }
                                        view.append("↑/↓ select · Tab complete · Enter run · Esc close\nF2 permissions · F3 subagents · F4 Compact/Cockpit\nQuestion: ↑/↓ + Enter, or type an answer\nCtrl-C cancels; twice quits · Ctrl-Q quits\nCtrl-Y allows once · Ctrl-N denies · PgUp/PgDn scroll\nAlt-Up/Down scroll request details · Ctrl-U clears input\n");
                                    },
                                    "/clear" => view.transcript.clear(),
                                    "/agents" => { view.screen = Screen::Agents; view.agent_scroll = 0; },
                                    "/permissions" => { view.screen = Screen::Permissions; view.detail_scroll = 0; },
                                    "/view" => view.open_appearance(),
                                    "/cancel" => { if let Some(run) = &active && !run.status().await?.status.terminal() { run.cancel("TUI /cancel", &id("cancel")).await?; } },
                                    _ => return Err(WorkerError::new("unknown_command", "Unknown command. Type / for suggestions or /help.")),
                                }
                                view.input.clear(); view.edited_input(); view.scroll = 0;
                                return Ok(());
                            }
                            if view.approval.is_some() {
                                return Err(WorkerError::new("approval_pending", "Use Ctrl-Y to allow this action or Ctrl-N to deny. F2 shows permissions."));
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
                            view.append(&format!("\n\nyou › {input}\n")); view.input.clear(); view.edited_input(); view.scroll = 0;
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
            terminal.draw(|f| draw(f, &view)).unwrap();
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
        terminal.draw(|f| draw(f, &view)).unwrap();
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
            terminal.draw(|f| draw(f, &view)).unwrap();
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
            Screen::Appearance,
        ] {
            view.screen = screen;
            for (width, height) in [(130, 36), (40, 12), (1, 1)] {
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                terminal.draw(|f| draw(f, &view)).unwrap();
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
                        Screen::Appearance => "Choose your layout",
                    }));
                }
            }
        }
    }
}
