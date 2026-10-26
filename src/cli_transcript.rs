//! Fenced code in a quiet, warm palette. Text is already terminal-sanitized.
use ratatui::{
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
};
use std::collections::BTreeMap;
use unicode_segmentation::UnicodeSegmentation;

/// Presentation only: IDs are absolute transcript byte positions, so streamed
/// text and bounded history pruning cannot transfer a choice to another block.
#[derive(Default)]
pub(crate) struct Folds {
    all: Option<bool>,
    overrides: BTreeMap<usize, bool>,
    boundaries: Vec<usize>,
}
impl Folds {
    fn expanded(&self, id: usize, rows: usize) -> bool {
        self.overrides
            .get(&id)
            .copied()
            .or(self.all)
            .unwrap_or(rows <= 8)
    }
    pub(crate) fn set(&mut self, id: usize, expanded: bool) {
        self.overrides.insert(id, expanded);
    }
    pub(crate) fn set_all(&mut self, expanded: bool) {
        self.all = Some(expanded);
        self.overrides.clear();
    }
    // UI messages cannot become part of an unfinished model code fence.
    pub(crate) fn boundary(&mut self, offset: usize) {
        if self.boundaries.last() != Some(&offset) {
            self.boundaries.push(offset);
        }
    }
    pub(crate) fn prune(&mut self, before: usize) {
        self.overrides.retain(|id, _| *id >= before);
        self.boundaries.retain(|offset| *offset >= before);
    }
    pub(crate) fn clear(&mut self) {
        self.overrides.clear();
        self.boundaries.clear();
    }
}

#[derive(Clone, Copy)]
pub(crate) struct FoldHeader {
    pub id: usize,
    pub row: usize,
    pub height: usize,
    pub expanded: bool,
}
pub(crate) struct Rendered {
    pub text: Text<'static>,
    pub headers: Vec<FoldHeader>,
}

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
            (rest.graphemes(true).next().unwrap().len(), CODE_TEXT)
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

// Bound previews by terminal cells, including minified code, tabs and wide or
// combining characters. The expanded block still renders the original source.
fn preview(source: &str, width: usize) -> String {
    if Span::raw(source).width() <= width {
        return source.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 0;
    let mut result = String::new();
    for grapheme in source.graphemes(true) {
        used += Span::raw(grapheme).width();
        if used >= width {
            break;
        }
        result.push_str(grapheme);
    }
    result.push('…');
    result
}

pub(crate) fn render(source: &str, width: u16, base: usize, folds: &Folds) -> Rendered {
    let mut lines = Vec::new();
    let mut rows = 0;
    let mut headers = Vec::new();
    let mut source_lines = Vec::new();
    let mut start = 0;
    for end in folds
        .boundaries
        .iter()
        .copied()
        .filter(|offset| *offset > base && *offset < base + source.len())
        .map(|offset| offset - base)
        .chain(std::iter::once(source.len()))
    {
        let section = &source[start..end];
        let mut offset = base + start;
        let mut parts: Vec<_> = section.split('\n').collect();
        if end < source.len() && section.ends_with('\n') {
            parts.pop();
        }
        for line in parts {
            source_lines.push((offset, line, start));
            offset += line.len() + 1;
        }
        start = end;
    }
    let source = source_lines;
    let push = |line: Line<'static>, lines: &mut Vec<Line<'static>>, rows: &mut usize| {
        *rows += Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width.max(1));
        lines.push(line);
    };
    let fence = |raw: &str| {
        let trimmed = raw.trim_start();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let count = trimmed.chars().take_while(|c| *c == marker).count();
        (matches!(marker, '`' | '~') && count >= 3).then_some((marker, count))
    };
    let mut index = 0;
    while index < source.len() {
        let (id, mut raw, section) = source[index];
        // The model may start its response directly with a code fence.
        if let Some(after) = raw.strip_prefix("werk › ")
            && fence(after).is_some()
        {
            push(Line::from("werk ›"), &mut lines, &mut rows);
            raw = after;
        }
        let Some((marker, count)) = fence(raw) else {
            push(Line::from(raw.to_owned()), &mut lines, &mut rows);
            index += 1;
            continue;
        };
        let language = raw.trim_start()[count..]
            .split_whitespace()
            .next()
            .unwrap_or("code");
        let section_end = (index + 1..source.len())
            .find(|&n| source[n].2 != section)
            .unwrap_or(source.len());
        let end = (index + 1..section_end).find(|&n| {
            fence(source[n].1).is_some_and(|(close, length)| {
                close == marker
                    && length >= count
                    && source[n].1.trim_start()[length..].trim().is_empty()
            })
        });
        let mut body_end = end.unwrap_or(section_end);
        if end.is_none() && body_end > index + 1 && source[body_end - 1].1.is_empty() {
            body_end -= 1;
        }
        let count = body_end - index - 1;
        let mut body_rows = 0;
        for (_, line, _) in &source[index + 1..body_end] {
            body_rows += Paragraph::new(format!("│ {}", line.replace('\t', "    ")))
                .wrap(Wrap { trim: false })
                .line_count(width.max(1));
            if body_rows > 8 {
                break;
            }
        }
        let expanded = folds.expanded(id, body_rows);
        let row = rows;
        push(
            Line::styled(
                format!(
                    "{} {} · {count} lines · {}{}",
                    if expanded { "▼" } else { "▶" },
                    language.chars().take(24).collect::<String>(),
                    if expanded { "collapse" } else { "expand" },
                    if end.is_some() {
                        ""
                    } else if section_end < source.len() {
                        " · unfinished"
                    } else {
                        " · streaming"
                    }
                ),
                Style::default().fg(EDGE).bg(CODE_BG),
            ),
            &mut lines,
            &mut rows,
        );
        headers.push(FoldHeader {
            id,
            row,
            height: rows - row,
            expanded,
        });
        for (_, line, _) in
            source[index + 1..body_end]
                .iter()
                .take(if expanded { count } else { 3 })
        {
            let line = if expanded {
                code_line(line, width, language)
            } else if width < 2 {
                Line::from(preview("│ ", width.into()))
            } else {
                code_line(
                    &preview(&line.replace('\t', "    "), usize::from(width - 2)),
                    width,
                    language,
                )
            };
            push(line, &mut lines, &mut rows);
        }
        if !expanded && count > 3 {
            push(
                Line::styled(
                    format!("│ … {} more lines", count - 3),
                    Style::default().fg(COMMENT).bg(CODE_BG),
                ),
                &mut lines,
                &mut rows,
            );
        }
        if end.is_some() {
            push(
                Line::styled("╰─", Style::default().fg(EDGE)),
                &mut lines,
                &mut rows,
            );
        }
        index = end.map_or(section_end, |n| n + 1);
    }
    Rendered {
        text: Text::from(lines),
        headers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible(rendered: &Rendered) -> String {
        rendered.text.to_string()
    }

    #[test]
    fn streaming_blocks_keep_individual_choices_and_restore_full_content() {
        let mut folds = Folds::default();
        let mut source = "werk › ```rust\n".to_owned() + &"    let value = 1;\n".repeat(10);
        let folded = render(&source, 80, 50, &folds);
        let id = folded.headers[0].id;
        assert!(!folded.headers[0].expanded);
        assert!(visible(&folded).contains("10 lines · expand · streaming"));
        assert_eq!(visible(&folded).matches("let value").count(), 3);
        folds.set(id, true);
        source.push_str("    // final detail\n```\nAfter code\n~~~python\n");
        source.push_str(&"    value = 2\n".repeat(12));
        source.push_str("~~~");
        let opened = render(&source, 80, 50, &folds);
        assert_eq!(opened.headers[0].id, id);
        assert!(opened.headers[0].expanded);
        assert!(!opened.headers[1].expanded);
        assert!(visible(&opened).contains("│     // final detail"));
        assert!(visible(&opened).contains("After code"));
        assert_eq!(visible(&opened).matches("let value").count(), 10);
        folds.set_all(true);
        assert_eq!(
            visible(&render(&source, 80, 50, &folds))
                .matches("value = 2")
                .count(),
            12
        );
        folds.set_all(false);
        assert!(
            render(&source, 80, 50, &folds)
                .headers
                .iter()
                .all(|h| !h.expanded)
        );
    }

    #[test]
    fn minified_and_unicode_previews_are_bounded_by_display_width() {
        let source = format!("```js\n\t{}\n```", "世界e\u{301}👩‍💻".repeat(100));
        let mut folds = Folds::default();
        let folded = render(&source, 32, 0, &folds);
        assert!(!folded.headers[0].expanded);
        let preview_line = &folded.text.lines[1];
        assert!(preview_line.width() <= 32);
        assert!(preview_line.to_string().contains('…'));
        assert!(visible(&folded).contains("世界e\u{301}👩‍💻"));
        folds.set_all(true);
        let expanded = render(&source, 32, 0, &folds);
        assert_eq!(visible(&expanded).matches("世界e\u{301}👩‍💻").count(), 100);
        assert!(visible(&expanded).contains("│     世界"));
        assert_eq!(preview("世界", 1), "…");
        assert_eq!(preview("e\u{301}abc", 3), "e\u{301}a…");
    }

    #[test]
    fn fence_boundaries_and_history_pruning_do_not_reuse_another_blocks_choice() {
        let source = "````text\none\n```\n~~~\ntwo\n````\nafter\n```rust\nthree\n```";
        let mut folds = Folds::default();
        folds.set(0, false);
        let rendered = render(source, 80, 0, &folds);
        assert_eq!(rendered.headers.len(), 2);
        assert!(!rendered.headers[0].expanded);
        assert!(rendered.headers[1].expanded);
        assert!(visible(&rendered).contains("4 lines"));
        folds.prune(1);
        assert!(folds.overrides.is_empty());
        assert!(render(source, 80, 100, &folds).headers[0].expanded);
        folds.set(0, false);
        folds.clear();
        assert!(render(source, 80, 0, &folds).headers[0].expanded);
    }
}
