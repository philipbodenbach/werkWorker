//! Host clipboard access and selection of visible terminal text.
use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
use std::{io, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use unicode_segmentation::UnicodeSegmentation;

pub(super) const INPUT_LIMIT: usize = 65_536;

pub(super) struct PastedText {
    pub marker: String,
    pub text: String,
}

/// Byte offsets for whole graphemes; collapsed pastes behave like single tokens.
pub(super) fn boundaries(input: &str, pastes: &[PastedText]) -> Vec<usize> {
    let ranges: Vec<_> = pastes
        .iter()
        .flat_map(|paste| {
            input
                .match_indices(&paste.marker)
                .map(|(start, _)| start..start + paste.marker.len())
        })
        .collect();
    input
        .grapheme_indices(true)
        .map(|(i, _)| i)
        .chain(std::iter::once(input.len()))
        .filter(|i| {
            !ranges
                .iter()
                .any(|range| range.start < *i && *i < range.end)
        })
        .collect()
}

pub(super) struct InputLayout {
    pub lines: Vec<String>,
    pub positions: Vec<(usize, usize, usize)>,
}

/// Use the same wrapping for painting and cursor placement, including wide
/// graphemes, explicit newlines and the empty cell after a full final row.
pub(super) fn input_layout(input: &str, width: usize) -> InputLayout {
    let width = width.max(1);
    let mut lines = vec![String::new()];
    let mut column = 0;
    let mut positions = Vec::new();
    for (offset, grapheme) in "› "
        .graphemes(true)
        .map(|g| (None, g))
        .chain(input.grapheme_indices(true).map(|(i, g)| (Some(i), g)))
    {
        let shown = if grapheme == "\t" { "    " } else { grapheme };
        let cells = ratatui::text::Span::raw(shown).width();
        if grapheme != "\n" && column + cells > width && column > 0 {
            lines.push(String::new());
            column = 0;
        }
        if let Some(offset) = offset {
            positions.push((offset, column, lines.len() - 1));
        }
        if grapheme == "\n" {
            lines.push(String::new());
            column = 0;
        } else {
            lines.last_mut().unwrap().push_str(shown);
            column += cells.min(width);
        }
    }
    if column >= width {
        lines.push(String::new());
        column = 0;
    }
    positions.push((input.len(), column, lines.len() - 1));
    InputLayout { lines, positions }
}

/// Expand only markers in the draft itself, never marker-like text inside a paste.
pub(super) fn expand_pastes(input: &str, pastes: &[PastedText]) -> String {
    let mut expanded = String::new();
    let mut rest = input;
    while let Some((at, paste)) = pastes
        .iter()
        .filter_map(|paste| rest.find(&paste.marker).map(|at| (at, paste)))
        .min_by_key(|(at, _)| *at)
    {
        expanded.push_str(&rest[..at]);
        expanded.push_str(&paste.text);
        rest = &rest[at + paste.marker.len()..];
    }
    expanded.push_str(rest);
    expanded
}

pub(super) fn append_paste(input: &mut String, value: &str) -> bool {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    for c in normalized
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
    {
        if input.len() + c.len_utf8() > INPUT_LIMIT {
            return false;
        }
        input.push(c);
    }
    true
}

pub(super) struct Selection {
    pub area: Rect,
    pub start: (u16, u16),
    pub end: (u16, u16),
}

impl Selection {
    pub fn extend(&mut self, x: u16, y: u16) {
        self.end = (
            x.clamp(self.area.x, self.area.right() - 1),
            y.clamp(self.area.y, self.area.bottom() - 1),
        );
    }

    fn bounds(&self) -> ((u16, u16), (u16, u16)) {
        if (self.start.1, self.start.0) <= (self.end.1, self.end.0) {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }

    pub fn text(&self, buffer: &Buffer) -> String {
        let (start, end) = self.bounds();
        (start.1..=end.1)
            .map(|y| {
                let left = if y == start.1 { start.0 } else { self.area.x };
                let right = if y == end.1 {
                    end.0 + 1
                } else {
                    self.area.right()
                };
                let mut text = String::new();
                let mut x = self.area.x;
                while x < right {
                    let cell = &buffer[(x, y)];
                    let width = ratatui::text::Span::raw(cell.symbol()).width().max(1) as u16;
                    if x >= left || x.saturating_add(width) > left {
                        text.push_str(cell.symbol());
                    }
                    x = x.saturating_add(width);
                }
                text.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn highlight(&self, buffer: &mut Buffer) {
        let (start, end) = self.bounds();
        for y in start.1..=end.1 {
            let left = if y == start.1 { start.0 } else { self.area.x };
            let right = if y == end.1 {
                end.0 + 1
            } else {
                self.area.right()
            };
            for x in left..right {
                buffer[(x, y)]
                    .set_style(ratatui::style::Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

fn commands(write: bool) -> Vec<(String, Vec<String>)> {
    let mut commands = Vec::new();
    let mut add = |program: &str, args: &[&str]| {
        commands.push((program.into(), args.iter().map(|s| (*s).into()).collect()));
    };
    if cfg!(windows)
        || std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::env::var_os("WSL_INTEROP").is_some()
    {
        // Clipboard contents travel through stdin/stdout, never through command text.
        let script = if write {
            "$ErrorActionPreference='Stop'; [Console]::InputEncoding=[Text.UTF8Encoding]::new($false); Set-Clipboard -Value ([Console]::In.ReadToEnd())"
        } else {
            "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); [Console]::Write((Get-Clipboard -Raw))"
        };
        let args = [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-Command",
            script,
        ];
        add("powershell.exe", &args);
        if cfg!(target_os = "linux") {
            add(
                "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe",
                &args,
            );
        }
    }
    if cfg!(target_os = "macos") {
        add(if write { "pbcopy" } else { "pbpaste" }, &[]);
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        if write {
            add("wl-copy", &["--type", "text/plain;charset=utf-8"]);
        } else {
            add("wl-paste", &["--no-newline"]);
        }
    }
    if std::env::var_os("DISPLAY").is_some() {
        add(
            "xclip",
            &[
                "-selection",
                "clipboard",
                if write { "-in" } else { "-out" },
            ],
        );
        add(
            "xsel",
            &["--clipboard", if write { "--input" } else { "--output" }],
        );
    }
    commands
}

async fn transfer(program: &str, args: &[String], value: Option<&str>) -> io::Result<String> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(if value.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if value.is_some() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        if let Some(value) = value {
            let mut stdin = child.stdin.take().expect("piped stdin");
            stdin.write_all(value.as_bytes()).await?;
            stdin.shutdown().await?;
        }
        let mut bytes = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            stdout
                .take((INPUT_LIMIT + 1) as u64)
                .read_to_end(&mut bytes)
                .await?;
        }
        if bytes.len() > INPUT_LIMIT {
            return Err(io::Error::other("Clipboard text exceeds 64 KiB"));
        }
        if !child.wait().await?.success() {
            return Err(io::Error::other("Clipboard helper failed"));
        }
        String::from_utf8(bytes).map_err(io::Error::other)
    })
    .await;
    result.unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Clipboard helper timed out",
        ))
    })
}

pub(super) async fn paste() -> io::Result<String> {
    let mut last = io::Error::other("No clipboard helper available");
    for (program, args) in commands(false) {
        match transfer(&program, &args, None).await {
            Ok(text) => return Ok(text),
            Err(error) => last = error,
        }
    }
    Err(last)
}

pub(super) async fn copy(value: &str) -> io::Result<()> {
    for (program, args) in commands(true) {
        if transfer(&program, &args, Some(value)).await.is_ok() {
            return Ok(());
        }
    }
    // Supports remote terminals without a local graphical clipboard service.
    crossterm::execute!(
        std::io::stdout(),
        crossterm::clipboard::CopyToClipboard::to_clipboard_from(value),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_normalizes_lines_filters_controls_and_limits_utf8_bytes() {
        let mut input = "draft ".to_string();
        assert!(append_paste(&mut input, "Grüße\r\n世界\r\tend\x03\x1b"));
        assert_eq!(input, "draft Grüße\n世界\n\tend");
        input = "x".repeat(INPUT_LIMIT - 1);
        assert!(!append_paste(&mut input, "界"));
        assert_eq!(input.len(), INPUT_LIMIT - 1);
        assert!(append_paste(&mut input, "z"));
        assert_eq!(input.len(), INPUT_LIMIT);
    }

    #[test]
    fn selection_is_confined_to_one_pane_and_handles_reverse_drag_and_wide_text() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 4));
        buffer.set_string(1, 1, "Hello 世界", ratatui::style::Style::default());
        buffer.set_string(1, 2, "second", ratatui::style::Style::default());
        buffer.set_string(12, 1, "sidebar", ratatui::style::Style::default());
        let mut selection = Selection {
            area: Rect::new(1, 1, 10, 2),
            start: (6, 2),
            end: (1, 1),
        };
        assert_eq!(selection.text(&buffer), "Hello 世界\nsecond");
        selection.extend(19, 3);
        assert_eq!(selection.end, (10, 2));
        assert_eq!(selection.text(&buffer), "d");
    }
}
