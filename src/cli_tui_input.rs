//! Host clipboard access and selection of visible terminal text.
use base64::{Engine, engine::general_purpose::STANDARD};
use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
use std::{io, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use unicode_segmentation::UnicodeSegmentation;
use werk_worker::attachments::{CLIPBOARD_IMAGE_LIMIT, ImageAttachment};

pub(super) enum Clipboard {
    Text(String),
    Image(ImageAttachment),
}

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
            "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); Add-Type -AssemblyName System.Windows.Forms; if ([Windows.Forms.Clipboard]::ContainsImage()) { $i=[Windows.Forms.Clipboard]::GetImage(); $s=[IO.MemoryStream]::new(); try { $i.Save($s,[Drawing.Imaging.ImageFormat]::Png); [Console]::Write('WERK_IMAGE_PNG:'+ [Convert]::ToBase64String($s.ToArray())) } finally { $s.Dispose(); $i.Dispose() } } else { [Console]::Write([Windows.Forms.Clipboard]::GetText()) }"
        };
        let args = [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-Command",
            script,
        ];
        let in_path = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|p| p.join("powershell.exe").is_file())
        });
        add(
            if in_path || cfg!(windows) {
                "powershell.exe"
            } else {
                "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"
            },
            &args,
        );
    }
    if cfg!(target_os = "macos") {
        if !write {
            add(
                "osascript",
                &[
                    "-l",
                    "JavaScript",
                    "-e",
                    "ObjC.import('AppKit'); ObjC.import('Foundation'); var p=$.NSPasteboard.generalPasteboard; var d=p.dataForType('public.png'); if (!d) { var t=p.dataForType('public.tiff'); if(t) { var r=$.NSBitmapImageRep.imageRepWithData(t); d=r.representationUsingTypeProperties($.NSBitmapImageFileTypePNG,$.NSDictionary.dictionary); } } if(d) { 'WERK_IMAGE_PNG:'+ObjC.unwrap(d.base64EncodedStringWithOptions(0)); } else { ObjC.unwrap(p.stringForType('public.utf8-plain-text')) || ''; }",
                ],
            );
        }
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

async fn transfer_bytes(
    program: &str,
    args: &[String],
    value: Option<&str>,
    limit: usize,
) -> io::Result<Vec<u8>> {
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
                .take((limit + 1) as u64)
                .read_to_end(&mut bytes)
                .await?;
        }
        if bytes.len() > limit {
            return Err(io::Error::other("Clipboard exceeds the import size limit"));
        }
        if !child.wait().await?.success() {
            return Err(io::Error::other("Clipboard helper failed"));
        }
        Ok(bytes)
    })
    .await;
    result.unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Clipboard helper timed out",
        ))
    })
}

fn decode_clipboard(bytes: Vec<u8>) -> io::Result<Clipboard> {
    let image = if let Some(encoded) = bytes.strip_prefix(b"WERK_IMAGE_PNG:") {
        Some(
            STANDARD
                .decode(encoded.trim_ascii())
                .map_err(io::Error::other)?,
        )
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(bytes.clone())
    } else {
        None
    };
    if let Some(image) = image {
        return ImageAttachment::from_bytes(&image)
            .map(Clipboard::Image)
            .map_err(io::Error::other);
    }
    if bytes.len() > INPUT_LIMIT {
        return Err(io::Error::other("Clipboard text exceeds 64 KiB"));
    }
    String::from_utf8(bytes)
        .map(Clipboard::Text)
        .map_err(io::Error::other)
}

pub(super) async fn paste() -> io::Result<Clipboard> {
    let mut last = io::Error::other("No clipboard helper available");
    for (program, args) in commands(false) {
        // X11 defaults to text targets. Ask for an actual image when present.
        let mut args = args;
        if program == "xclip" {
            let targets: Vec<String> = ["-selection", "clipboard", "-out", "-target", "TARGETS"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            if let Ok(types) = transfer_bytes(&program, &targets, None, 8192).await
                && String::from_utf8_lossy(&types)
                    .lines()
                    .any(|t| t.trim() == "image/png")
            {
                args.extend(["-target".into(), "image/png".into()]);
            }
        }
        match transfer_bytes(&program, &args, None, CLIPBOARD_IMAGE_LIMIT * 4 / 3 + 128).await {
            Ok(bytes) => {
                return tokio::task::spawn_blocking(move || decode_clipboard(bytes))
                    .await
                    .map_err(io::Error::other)?;
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}

pub(super) async fn copy(value: &str) -> io::Result<()> {
    for (program, args) in commands(true) {
        if transfer_bytes(&program, &args, Some(value), INPUT_LIMIT)
            .await
            .is_ok()
        {
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
