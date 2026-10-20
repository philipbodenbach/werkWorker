//! Keyboard-operated connection and durable session selection.
use crate::cli_tui::{BG, BRAND, MUTED, Restore, TEXT, clean, panel};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    layout::{Constraint, Layout},
    style::Style,
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{future::Future, path::Path};
use werk_worker::{sessions::SessionStore, *};

pub async fn choose(title: &str, detail: &str, options: &[String]) -> Result<Option<usize>> {
    let mut terminal = ratatui::try_init()?;
    let _restore = Restore;
    let mut keys = crate::cli_tui::input().await;
    let mut state = ListState::default().with_selected(Some(0));
    loop {
        terminal.draw(|frame| {
            frame.render_widget(
                Block::default().style(Style::default().bg(BG).fg(TEXT)),
                frame.area(),
            );
            let rows = Layout::vertical([
                Constraint::Length(2),
                Constraint::Length(5),
                Constraint::Min(3),
                Constraint::Length(2),
            ])
            .split(frame.area());
            frame.render_widget(
                Paragraph::new(" werkWorker").style(Style::default().fg(BRAND[0])),
                rows[0],
            );
            frame.render_widget(
                Paragraph::new(clean(detail)).wrap(Wrap { trim: false }),
                rows[1],
            );
            let items = options
                .iter()
                .map(|s| ListItem::new(clean(s)))
                .collect::<Vec<_>>();
            frame.render_stateful_widget(
                List::new(items)
                    .block(panel(title, BRAND[4]))
                    .highlight_symbol("› ")
                    .highlight_style(Style::default().fg(BRAND[4]).bold()),
                rows[2],
                &mut state,
            );
            frame.render_widget(
                Paragraph::new(" ↑/↓ select · Enter continue · Esc / Ctrl-Q close")
                    .style(Style::default().fg(MUTED)),
                rows[3],
            );
        })?;
        let Some(event) = keys.next().await else {
            return Ok(None);
        };
        if let Event::Key(key) = event? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.code == KeyCode::Esc
                || (key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('q' | 'c')))
            {
                return Ok(None);
            }
            let selected = state.selected().unwrap_or(0);
            match key.code {
                KeyCode::Up if !options.is_empty() => {
                    state.select(Some((selected + options.len() - 1) % options.len()))
                }
                KeyCode::Down if !options.is_empty() => {
                    state.select(Some((selected + 1) % options.len()))
                }
                KeyCode::Enter if !options.is_empty() => return Ok(Some(selected)),
                _ => {}
            }
        }
    }
}

/// Dropping the future on cancellation also drops any in-flight HTTP requests.
pub async fn connecting<T>(
    detail: &str,
    future: impl Future<Output = Result<T>>,
) -> Result<Option<T>> {
    let mut terminal = ratatui::try_init()?;
    let _restore = Restore;
    terminal.draw(|frame| {
        frame.render_widget(
            Paragraph::new(format!(
                "{}\n\nConnecting to previous service…\nEsc / Ctrl-Q cancels.",
                clean(detail)
            ))
            .wrap(Wrap { trim: false })
            .block(panel("Continue previous", BRAND[4]))
            .style(Style::default().bg(BG).fg(TEXT)),
            frame.area(),
        );
    })?;
    let mut keys = crate::cli_tui::input().await;
    let pending = tokio::time::timeout(std::time::Duration::from_secs(20), future);
    tokio::pin!(pending);
    loop {
        tokio::select! {
            outcome = &mut pending => return outcome
                .map_err(|_| WorkerError::new("connection_timeout", "Connection timed out"))?
                .map(Some),
            event = keys.next() => {
                let Some(event) = event else { return Ok(None) };
                if let Event::Key(key) = event?
                    && key.kind == KeyEventKind::Press && (key.code == KeyCode::Esc
                        || (key.modifiers.contains(KeyModifiers::CONTROL)
                            && matches!(key.code, KeyCode::Char('q' | 'c')))) { return Ok(None); }
            }
        }
    }
}

pub enum SessionChoice {
    New,
    Previous(String),
    Back,
}

pub async fn sessions(
    store: &dyn SessionStore,
    workspace: &Path,
    always_show: bool,
) -> Result<SessionChoice> {
    let workspace = tokio::fs::canonicalize(workspace).await?;
    let mut entries = Vec::new();
    let mut unavailable = 0;
    for id in store.list().await? {
        let data = match store.load(&id).await {
            Ok(data) => data,
            Err(_) => {
                unavailable += 1;
                continue;
            }
        };
        if data.parent.is_some() || data.workspace != workspace || data.runs.is_empty() {
            continue;
        }
        let last = data.events.last().map_or(0, |e| e.timestamp_ms);
        let title = data
            .runs
            .first()
            .map(|r| r.input.as_str())
            .unwrap_or("Empty conversation")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(70)
            .collect::<String>();
        let status = data
            .runs
            .last()
            .map_or("Ready".into(), |r| format!("{:?}", r.status));
        let age = now_ms().saturating_sub(last) / 60_000;
        let age = if age < 60 {
            format!("{age}m ago")
        } else if age < 1440 {
            format!("{}h ago", age / 60)
        } else {
            format!("{}d ago", age / 1440)
        };
        entries.push((
            last,
            id,
            format!(
                "{title}\n  {age} · {status} · {} · {} turns",
                data.profile.name,
                data.runs.len()
            ),
        ));
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    if entries.is_empty() && !always_show && unavailable == 0 {
        return Ok(SessionChoice::New);
    }
    let mut options = vec!["New session\n  Start a new conversation".into()];
    options.extend(
        entries
            .iter()
            .map(|(_, id, label)| format!("{label}\n  {id}")),
    );
    let mut detail = format!(
        "{}\nPrevious conversations, most recent first.\nOpening a session restores its history, profile and permissions.",
        workspace.display()
    );
    if unavailable > 0 {
        detail.push_str(&format!(
            "\n{unavailable} unreadable session(s) omitted; stored data was preserved."
        ));
    }
    Ok(
        match choose("New / Previous sessions", &detail, &options).await? {
            None => SessionChoice::Back,
            Some(0) => SessionChoice::New,
            Some(index) => SessionChoice::Previous(entries[index - 1].1.clone()),
        },
    )
}
