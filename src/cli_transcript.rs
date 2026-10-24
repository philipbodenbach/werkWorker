//! Fenced code in a quiet, warm palette. Text is already terminal-sanitized.
use ratatui::{
    style::{Color, Style},
    text::{Line, Span, Text},
};

pub(crate) const CODE_BG: Color = Color::Rgb(29, 32, 30);
const CODE_TEXT: Color = Color::Rgb(213, 210, 193);
const COMMENT: Color = Color::Rgb(139, 148, 129);
const STRING: Color = Color::Rgb(169, 185, 141);
const NUMBER: Color = Color::Rgb(205, 171, 120);
const KEYWORD: Color = Color::Rgb(183, 157, 174);
const EDGE: Color = Color::Rgb(121, 139, 119);

fn code_line(source: &str, width: u16, language: &str) -> Line<'static> {
    let source = source.replace('\t', "    ");
    let mut spans = vec![Span::styled("│ ", Style::default().fg(EDGE))];
    let mut rest = source.as_str();
    while !rest.is_empty() {
        let first = rest.chars().next().unwrap();
        let (length, color) = if rest.starts_with("//")
            || (rest.starts_with('#')
                && matches!(
                    language,
                    "python" | "py" | "sh" | "bash" | "toml" | "yaml" | "yml"
                )) {
            (rest.len(), COMMENT)
        } else if matches!(first, '"' | '\'') {
            let mut escaped = false;
            let end = rest.char_indices().skip(1).find_map(|(i, c)| {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == first {
                    return Some(i + c.len_utf8());
                }
                None
            });
            // A Rust lifetime isn't a quoted string. An unfinished double quote
            // can still be highlighted while its response is streaming.
            match end {
                Some(end) => (end, STRING),
                None if first == '"' => (rest.len(), STRING),
                None => (1, CODE_TEXT),
            }
        } else if first.is_alphanumeric() || first == '_' {
            let end = rest
                .char_indices()
                .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
                .map_or(rest.len(), |(i, _)| i);
            let token = &rest[..end];
            let color = if first.is_ascii_digit() {
                NUMBER
            } else if matches!(
                token,
                "fn" | "let"
                    | "mut"
                    | "pub"
                    | "use"
                    | "mod"
                    | "impl"
                    | "trait"
                    | "struct"
                    | "enum"
                    | "match"
                    | "if"
                    | "else"
                    | "for"
                    | "while"
                    | "loop"
                    | "return"
                    | "break"
                    | "continue"
                    | "const"
                    | "static"
                    | "async"
                    | "await"
                    | "move"
                    | "self"
                    | "Self"
                    | "true"
                    | "false"
                    | "None"
                    | "Some"
                    | "Ok"
                    | "Err"
                    | "def"
                    | "class"
                    | "import"
                    | "from"
                    | "in"
                    | "with"
                    | "try"
                    | "except"
                    | "raise"
                    | "pass"
                    | "function"
                    | "var"
                    | "export"
                    | "default"
                    | "new"
                    | "null"
            ) {
                KEYWORD
            } else {
                CODE_TEXT
            };
            (end, color)
        } else {
            (first.len_utf8(), CODE_TEXT)
        };
        spans.push(Span::styled(
            rest[..length].to_owned(),
            Style::default().fg(color),
        ));
        rest = &rest[length..];
    }
    let mut line = Line::from(spans).style(Style::default().fg(CODE_TEXT).bg(CODE_BG));
    // Fill the rest of short code rows, retaining real spaces/indentation.
    let padding = usize::from(width).saturating_sub(line.width());
    line.spans.push(Span::raw(" ".repeat(padding)));
    line
}

pub(crate) fn render(source: &str, width: u16) -> Text<'static> {
    let mut lines = Vec::new();
    let mut fence: Option<(char, usize, String)> = None;
    for original in source.split('\n') {
        let mut raw = original;
        // The model may start its response directly with a code fence.
        if fence.is_none()
            && let Some(after) = raw.strip_prefix("werk › ")
            && (after.starts_with("```") || after.starts_with("~~~"))
        {
            lines.push(Line::from("werk ›"));
            raw = after;
        }
        let trimmed = raw.trim_start();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let count = trimmed.chars().take_while(|c| *c == marker).count();
        let is_fence = matches!(marker, '`' | '~') && count >= 3;
        if let Some((opening, length, language)) = &fence {
            if is_fence
                && marker == *opening
                && count >= *length
                && trimmed[count..].trim().is_empty()
            {
                lines.push(Line::styled("╰─", Style::default().fg(EDGE)));
                fence = None;
            } else {
                lines.push(code_line(raw, width, language));
            }
        } else if is_fence {
            let language = trimmed[count..].trim().to_owned();
            lines.push(Line::styled(
                format!(
                    "╭─ {}",
                    if language.is_empty() {
                        "code"
                    } else {
                        &language
                    }
                ),
                Style::default().fg(EDGE).bg(CODE_BG),
            ));
            fence = Some((marker, count, language));
        } else {
            lines.push(Line::from(raw.to_owned()));
        }
    }
    Text::from(lines)
}
