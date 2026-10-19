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
const BG: Color = Color::Rgb(9, 13, 23);
const PANEL: Color = Color::Rgb(15, 21, 35);
const TEXT: Color = Color::Rgb(224, 230, 248);
const MUTED: Color = Color::Rgb(129, 143, 174);
const BRAND: [Color; 5] = [
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
const COMMANDS: [(&str, &str); 4] = [
    ("/help", "Show keyboard shortcuts and commands"),
    ("/clear", "Clear the visible conversation"),
    ("/cancel", "Cancel the active run"),
    ("/quit", "Close werkWorker"),
];
fn banner() -> Vec<Line<'static>> {
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

struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        ratatui::restore();
    }
}
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
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
    configured: bool,
    model_requests: u32,
    tools: u32,
    command_selected: usize,
    commands_dismissed: bool,
    quit_armed: bool,
}
impl View {
    fn commands(&self) -> Vec<(&'static str, &'static str)> {
        if self.interaction.is_some() || self.commands_dismissed || !self.input.starts_with('/') {
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
            EventKind::ApprovalRequested { request } => {
                self.approval = Some(request);
                self.detail_scroll = 0;
            }
            EventKind::ApprovalResolved { .. } => self.approval = None,
            EventKind::InteractionRequested { request } => {
                self.interaction = Some(request);
                self.input.clear();
                self.detail_scroll = 0;
            }
            EventKind::InteractionAnswered { .. } => self.interaction = None,
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
fn panel(title: &str, color: Color) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(PANEL).fg(TEXT))
}
fn draw(frame: &mut Frame, view: &View) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(BG).fg(TEXT)),
        area,
    );
    let wide_banner = area.width >= 94 && area.height >= 24;
    let rows = Layout::vertical([
        Constraint::Length(if wide_banner { 8 } else { 2 }),
        Constraint::Min(4),
        Constraint::Length(4),
        Constraint::Length(2),
    ])
    .split(area);
    let logo = Line::from(vec![
        Span::styled(" w", Style::default().fg(BRAND[0]).bold()),
        Span::styled("e", Style::default().fg(BRAND[1]).bold()),
        Span::styled("r", Style::default().fg(BRAND[2]).bold()),
        Span::styled("k", Style::default().fg(BRAND[3]).bold()),
        Span::styled("Worker", Style::default().fg(BRAND[4]).bold()),
        Span::styled(
            format!("   AGENT  ·  {}", view.status),
            Style::default().fg(MUTED),
        ),
    ]);
    if wide_banner {
        frame.render_widget(Paragraph::new(banner()), rows[0]);
    } else {
        frame.render_widget(Paragraph::new(logo), rows[0]);
    }
    let columns =
        Layout::horizontal([Constraint::Percentage(68), Constraint::Percentage(32)]).split(rows[1]);
    let width = usize::from(columns[0].width.saturating_sub(2).max(1));
    let height = usize::from(columns[0].height.saturating_sub(2));
    let lines: usize = view
        .transcript
        .lines()
        .map(|line| line.chars().count().max(1).div_ceil(width))
        .sum();
    let offset = lines.saturating_sub(height).min(u16::MAX as usize) as u16;
    frame.render_widget(
        Paragraph::new(view.transcript.as_str())
            .wrap(Wrap { trim: false })
            .scroll((offset.saturating_sub(view.scroll), 0))
            .block(panel(" Conversation ", BRAND[0])),
        columns[0],
    );
    let (title, details) = if let Some(request) = &view.approval {
        (
            " Approval · Ctrl-Y allow / Ctrl-N deny ",
            serde_json::to_string_pretty(&request.description).unwrap_or_default(),
        )
    } else if let Some(request) = &view.interaction {
        (
            " Question · answer below ",
            format!("{}\n\n{}", request.question, request.choices.join("\n")),
        )
    } else {
        (
            " Session ",
            format!(
                "Status: {}\n{}\n\n{}\n\nModel requests: {}\nTool calls: {}\n\nEnter: send / steer\n/: commands\nCtrl-C: cancel; twice: quit\nCtrl-Q: close session\nPgUp/PgDn: scroll\nAlt-Up/Down: details\n\n{}",
                view.status,
                view.session_id,
                view.run_id,
                view.model_requests,
                view.tools,
                if view.configured {
                    "Resource grants and approvals are enforced by the engine."
                } else {
                    "Connect a model:\nww --werk-config config.json\n\nTry the offline UI:\nww tui --demo\n\nNo provider is connected."
                }
            ),
        )
    };
    frame.render_widget(
        Paragraph::new(clean(&details))
            .wrap(Wrap { trim: false })
            .scroll((view.detail_scroll, 0))
            .block(panel(
                title,
                if view.approval.is_some() {
                    BRAND[4]
                } else {
                    BRAND[3]
                },
            )),
        columns[1],
    );
    let input_title = if view.interaction.is_some() {
        " Your answer "
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
        Paragraph::new(format!("› {tail}")).block(panel(input_title, BRAND[1])),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(clean(if view.quit_armed {
            " Press Ctrl-C again to quit. Any other key continues."
        } else if view.error.is_empty() {
            " / commands   Ctrl-C cancel / twice quit   Ctrl-Q quit   Ctrl-Y allow   Ctrl-N deny"
        } else {
            &view.error
        }))
        .style(Style::default().fg(if view.error.is_empty() {
            MUTED
        } else {
            BRAND[4]
        })),
        rows[3],
    );
    let commands = view.commands();
    if !commands.is_empty() {
        let height = (commands.len() as u16 + 2).min(rows[2].y.saturating_sub(area.y));
        if height >= 3 {
            let popup = Rect::new(area.x, rows[2].y - height, area.width.min(76), height);
            let selected = view.command_selected.min(commands.len() - 1);
            let visible = usize::from(height - 2);
            let start = (selected + 1).saturating_sub(visible);
            let items: Vec<Line<'_>> = commands
                .iter()
                .enumerate()
                .skip(start)
                .take(visible)
                .map(|(i, (name, description))| {
                    Line::styled(
                        format!(
                            "{} {name:<9} {description}",
                            if i == selected { "›" } else { " " }
                        ),
                        if i == selected {
                            Style::default().fg(BG).bg(BRAND[0]).bold()
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
                    BRAND[0],
                )),
                popup,
            );
        }
    }
}

pub async fn show(
    session: Option<SessionHandle>,
    initial: Option<String>,
    resume: Option<RunHandle>,
) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(WorkerError::new(
            "terminal_required",
            "TUI requires an interactive terminal; use run or rpc for pipes",
        ));
    }
    let mut view = View {
        configured: session.is_some(),
        status: "Ready".into(),
        ..Default::default()
    };
    let mut active = resume;
    if let Some(session) = &session {
        let data = session.snapshot().await?;
        view.session_id = data.id;
        if let Some(run) = &active {
            let state = run.status().await?;
            for invocation in state.invocations {
                if let Some(request) = invocation.approval.filter(|r| r.decision.is_none()) {
                    view.approval = Some(request);
                }
                if let Some(request) = invocation.interaction.filter(|r| r.answer.is_none()) {
                    view.interaction = Some(request);
                }
            }
        }
        if let Some(input) = initial {
            view.append(&format!("you › {input}\n"));
            active = Some(
                session
                    .submit(input, &id("tui"), Limits::default(), None)
                    .await?,
            );
        }
    } else {
        view.append("Welcome to werkWorker.\n\nConnect Werk to start an agent, or choose the explicit offline demo.\nThe connection commands are shown in the session panel.");
        view.status = "Setup".into();
    }
    let mut events: Pin<Box<dyn Stream<Item = Result<WorkerEvent>> + Send>> = match &session {
        Some(session) => Box::pin(session.events(0)),
        None => Box::pin(futures_util::stream::pending()),
    };
    let mut terminal = ratatui::try_init()?;
    let _restore = Restore;
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut dirty = true;
    loop {
        tokio::select! {
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
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                if ctrl && key.code == KeyCode::Char('q') { break; }
                if ctrl && key.code == KeyCode::Char('c') {
                    if key.kind == KeyEventKind::Repeat { continue; }
                    if view.quit_armed { break; }
                    view.quit_armed = true;
                } else {
                    view.quit_armed = false;
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
                if key.code == KeyCode::Enter && view.interaction.is_none() && view.input == "/quit" { break; }
                let result: Result<()> = async {
                    if ctrl && key.code == KeyCode::Char('c') {
                        if let Some(run) = &active && !run.status().await?.status.terminal() { run.cancel("TUI Ctrl-C", &id("cancel")).await?; }
                    } else if ctrl && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('n')) {
                        if let (Some(run), Some(request)) = (&active, &view.approval) {
                            run.respond_to_approval(&request.request_id, if key.code == KeyCode::Char('y') { ApprovalDecision::AllowOnce } else { ApprovalDecision::Deny }, &id("approval")).await?;
                            view.approval = None;
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
                            if view.interaction.is_none() && input.starts_with('/') {
                                match input.as_str() {
                                    "/help" => {
                                        view.append("\n\nCommands:\n");
                                        for (name, description) in COMMANDS { view.append(&format!("{name}: {description}\n")); }
                                        view.append("↑/↓ select · Tab complete · Enter run · Esc close\nCtrl-C cancels; twice quits · Ctrl-Q quits\nCtrl-Y allows once · Ctrl-N denies · PgUp/PgDn scroll\nAlt-Up/Down scroll details\n");
                                    },
                                    "/clear" => view.transcript.clear(),
                                    "/cancel" => { if let Some(run) = &active && !run.status().await?.status.terminal() { run.cancel("TUI /cancel", &id("cancel")).await?; } },
                                    _ => return Err(WorkerError::new("unknown_command", "Unknown command. Type / for suggestions or /help.")),
                                }
                                view.input.clear(); view.edited_input(); view.scroll = 0;
                                return Ok(());
                            }
                            if let (Some(run), Some(request)) = (&active, &view.interaction) {
                                run.respond_to_interaction(&request.request_id, &input, &id("answer")).await?;
                                view.interaction = None;
                            } else if let Some(session) = &session {
                                if let Some(run) = &active && !run.status().await?.status.terminal() {
                                    run.steer(&input, &id("steer")).await?;
                                } else {
                                    active = Some(session.submit(&input, &id("tui"), Limits::default(), None).await?);
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
    Ok(())
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
        assert_eq!(view.commands().len(), 4);
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
        assert!(view.commands().is_empty());
        assert_eq!(view.input, "/ca");
        view.interaction = None;
        view.input = "/unknown".into();
        assert!(view.commands().is_empty());
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
            configured: true,
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
}
