//! Keyboard-operated connection and durable session selection.
use crate::cli_tui::{BG, BRAND, Restore, TEXT, choice_style, clean, panel, shortcut_text};
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
                .map(|s| ListItem::new(clean(s)).style(choice_style(false)))
                .collect::<Vec<_>>();
            frame.render_stateful_widget(
                List::new(items)
                    .block(panel(title, BRAND[4]))
                    .highlight_symbol("› ")
                    .highlight_style(choice_style(true)),
                rows[2],
                &mut state,
            );
            frame.render_widget(
                Paragraph::new(shortcut_text(
                    " ↑/↓ select · Enter continue · Esc / Ctrl-Q close",
                )),
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

#[derive(Clone)]
struct SessionEntry {
    id: String,
    title: String,
    label: String,
    modified: u64,
}

async fn session_entries(
    store: &dyn SessionStore,
    workspace: &Path,
) -> Result<(Vec<SessionEntry>, usize)> {
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
        if data.parent.is_some() || data.workspace != workspace {
            continue;
        }
        let last = data.events.last().map_or(0, |e| e.timestamp_ms);
        let title = data
            .title
            .as_deref()
            .or_else(|| data.runs.first().map(|r| r.input.as_str()))
            .unwrap_or("Empty conversation")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let title = if title.is_empty() {
            "Image conversation".into()
        } else {
            title.chars().take(80).collect::<String>()
        };
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
        let images: usize = data.messages.iter().map(|m| m.images.len()).sum();
        let label = format!(
            "{title}\n  {age} · {status} · {} · {} turns · {images} images\n  {id}",
            data.profile.name,
            data.runs.len()
        );
        entries.push(SessionEntry {
            id,
            title,
            label,
            modified: last,
        });
    }
    entries.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    Ok((entries, unavailable))
}

#[derive(Clone)]
enum SessionMode {
    Browse,
    Search,
    Rename {
        id: String,
        value: String,
    },
    Delete {
        id: String,
        title: String,
        confirm: bool,
    },
}

pub async fn sessions(
    store: &dyn SessionStore,
    engine: &WorkerEngine,
    workspace: &Path,
    always_show: bool,
) -> Result<SessionChoice> {
    let workspace = tokio::fs::canonicalize(workspace).await?;
    let (mut entries, mut unavailable) = session_entries(store, &workspace).await?;
    if entries.is_empty() && !always_show && unavailable == 0 {
        return Ok(SessionChoice::New);
    }
    let mut terminal = ratatui::try_init()?;
    let _restore = Restore;
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)?;
    let mut keys = crate::cli_tui::input().await;
    let mut state = ListState::default().with_selected(Some(0));
    let mut mode = SessionMode::Browse;
    let mut query = String::new();
    let mut notice = String::new();
    loop {
        let needle = query.to_lowercase();
        let visible: Vec<_> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.label.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        state.select(Some(state.selected().unwrap_or(0).min(visible.len())));
        terminal.draw(|frame| {
            frame.render_widget(Block::default().style(Style::default().bg(BG).fg(TEXT)),frame.area());
            let rows=Layout::vertical([Constraint::Length(2),Constraint::Length(5),Constraint::Min(3),Constraint::Length(3)]).split(frame.area());
            frame.render_widget(Paragraph::new(" werkWorker · Session management").style(Style::default().fg(BRAND[0])),rows[0]);
            let detail=match &mode {
                SessionMode::Rename {value,..} => format!("Rename session · 1..80 characters\nNew title: {value}▏\nEnter saves · Ctrl-U clears · Esc cancels"),
                SessionMode::Delete {id,title,..} => format!("Delete permanently? {}\n{id}\nHistory, images and subagent sessions will be removed. Workspace files and shared artifacts remain.",clean(title)),
                _ => format!("{}\nPrevious conversations, most recent first.\nSearch: {query}{}\n{} shown · {unavailable} unreadable session(s) preserved",workspace.display(),if matches!(mode,SessionMode::Search){"▏"}else{""},visible.len()),
            };
            frame.render_widget(Paragraph::new(clean(&detail)).wrap(Wrap{trim:false}),rows[1]);
            let items=std::iter::once(ListItem::new("New session\n  Start a new conversation".to_string())).chain(visible.iter().map(|i|ListItem::new(clean(&entries[*i].label)))).map(|item|item.style(choice_style(false))).collect::<Vec<_>>();
            if let SessionMode::Delete { confirm, .. } = &mode {
                let mut confirmation = ListState::default().with_selected(Some(usize::from(*confirm)));
                frame.render_stateful_widget(
                    List::new([ListItem::new("Cancel"), ListItem::new("Delete permanently")])
                        .block(panel("Confirm deletion", BRAND[4]))
                        .highlight_symbol("› ").highlight_style(choice_style(true)),
                    rows[2], &mut confirmation,
                );
            } else {
                frame.render_stateful_widget(List::new(items).block(panel("New / Previous sessions",BRAND[4])).highlight_symbol("› ").highlight_style(choice_style(true)),rows[2],&mut state);
            }
            let help=match mode {
                SessionMode::Browse => "↑/↓ select · Enter open · Del/D delete · R rename · / search · F5 refresh · Esc back",
                SessionMode::Search => "Type to filter · Enter selects results · Esc clears search",
                SessionMode::Rename {..} => "Enter save title · Ctrl-U clear · Esc cancel",
                SessionMode::Delete {..} => "↑/↓ choose · Enter confirm · Esc cancel",
            };
            frame.render_widget(Paragraph::new(shortcut_text(&format!("{help}\n{}",clean(&notice)))).wrap(Wrap{trim:false}),rows[3]);
        })?;
        let Some(event) = keys.next().await else {
            return Ok(SessionChoice::Back);
        };
        let event = event?;
        if let Event::Paste(text) = event {
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            match &mut mode {
                SessionMode::Rename { value, .. } => {
                    value.extend(
                        text.chars()
                            .filter(|c| !c.is_control())
                            .take(80usize.saturating_sub(value.chars().count())),
                    );
                }
                SessionMode::Search => {
                    query.extend(
                        text.chars()
                            .filter(|c| !c.is_control())
                            .take(80usize.saturating_sub(query.chars().count())),
                    );
                    state.select(Some(0));
                }
                _ => {}
            }
            continue;
        }
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('q' | 'c')) {
            return Ok(SessionChoice::Back);
        }
        let selected = state.selected().unwrap_or(0);
        let entry = selected
            .checked_sub(1)
            .and_then(|i| visible.get(i))
            .map(|i| entries[*i].clone());
        match mode.clone() {
            SessionMode::Delete {
                id,
                title,
                mut confirm,
            } => {
                match key.code {
                    KeyCode::Esc => {
                        mode = SessionMode::Browse;
                        notice = "Deletion cancelled.".into();
                        continue;
                    }
                    KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                        confirm = !confirm
                    }
                    KeyCode::Enter => {
                        if confirm {
                            match engine.delete_session(&id).await {
                                Ok(ids) => {
                                    notice = format!(
                                        "Deleted conversation and {} subagent session(s).",
                                        ids.len() - 1
                                    );
                                    (entries, unavailable) =
                                        session_entries(store, &workspace).await?;
                                }
                                Err(error) => notice = error.to_string(),
                            }
                        } else {
                            notice = "Deletion cancelled.".into();
                        }
                        mode = SessionMode::Browse;
                        continue;
                    }
                    _ => {}
                }
                mode = SessionMode::Delete { id, title, confirm };
            }
            SessionMode::Rename { id, mut value } => {
                match key.code {
                    KeyCode::Esc => {
                        mode = SessionMode::Browse;
                        notice = "Rename cancelled.".into();
                        continue;
                    }
                    KeyCode::Enter => match engine.rename_session(&id, &value).await {
                        Ok(()) => {
                            (entries, unavailable) = session_entries(store, &workspace).await?;
                            notice = "Session renamed.".into();
                            mode = SessionMode::Browse;
                            continue;
                        }
                        Err(error) => notice = error.to_string(),
                    },
                    KeyCode::Backspace => {
                        value.pop();
                    }
                    KeyCode::Char('u') if ctrl => value.clear(),
                    KeyCode::Char(c) if !ctrl && !c.is_control() && value.chars().count() < 80 => {
                        value.push(c)
                    }
                    _ => {}
                }
                mode = SessionMode::Rename { id, value };
            }
            SessionMode::Search => {
                match key.code {
                    KeyCode::Esc => {
                        query.clear();
                        mode = SessionMode::Browse;
                    }
                    KeyCode::Enter => mode = SessionMode::Browse,
                    KeyCode::Backspace => {
                        query.pop();
                    }
                    KeyCode::Char('u') if ctrl => query.clear(),
                    KeyCode::Char(c) if !ctrl && !c.is_control() && query.chars().count() < 80 => {
                        query.push(c)
                    }
                    _ => {}
                }
                state.select(Some(0));
            }
            SessionMode::Browse => match key.code {
                KeyCode::Esc if !query.is_empty() => {
                    query.clear();
                    state.select(Some(0));
                }
                KeyCode::Esc => return Ok(SessionChoice::Back),
                KeyCode::Up => state.select(Some((selected + visible.len()) % (visible.len() + 1))),
                KeyCode::Down => state.select(Some((selected + 1) % (visible.len() + 1))),
                KeyCode::Enter => {
                    return Ok(entry.map_or(SessionChoice::New, |e| SessionChoice::Previous(e.id)));
                }
                KeyCode::Char('/') => {
                    mode = SessionMode::Search;
                    notice.clear();
                }
                KeyCode::F(5) => {
                    (entries, unavailable) = session_entries(store, &workspace).await?;
                    notice = "Session list refreshed.".into();
                }
                KeyCode::Delete | KeyCode::Char('d' | 'D') if entry.is_some() => {
                    let e = entry.unwrap();
                    mode = SessionMode::Delete {
                        id: e.id,
                        title: e.title,
                        confirm: false,
                    };
                    notice.clear();
                }
                KeyCode::Char('r' | 'R') if entry.is_some() => {
                    let e = entry.unwrap();
                    mode = SessionMode::Rename {
                        id: e.id,
                        value: e.title,
                    };
                    notice.clear();
                }
                _ => {}
            },
        }
    }
}
