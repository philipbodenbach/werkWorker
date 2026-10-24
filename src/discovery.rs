//! Bounded workspace discovery with explicit continuation and incomplete results.
use crate::{core::*, tools::ToolContext};
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};

fn pattern(value: &str, glob: bool, insensitive: bool) -> Result<Regex> {
    if value.len() > 1024 {
        return Err(WorkerError::new(
            "invalid_pattern",
            "Use a pattern of at most 1024 bytes",
        ));
    }
    let source = if glob {
        let mut out: String = if value.contains('/') {
            "^".into()
        } else {
            "(?:^|/)".into()
        };
        let mut chars = value.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '*' if chars.peek() == Some(&'*') => {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        out.push_str("(?:.*/)?");
                    } else {
                        out.push_str(".*");
                    }
                }
                '*' => out.push_str("[^/]*"),
                '?' => out.push_str("[^/]"),
                '[' | ']' | '{' | '}' => {
                    return Err(WorkerError::new(
                        "invalid_pattern",
                        "Supported globs: *, **, ?. Use one positive pattern, e.g. **/*.js",
                    ));
                }
                c => out.push_str(&regex::escape(&c.to_string())),
            }
        }
        out.push('$');
        out
    } else {
        value.into()
    };
    RegexBuilder::new(&source)
        .case_insensitive(insensitive)
        .size_limit(2 * 1024 * 1024)
        .build()
        .map_err(|_| {
            WorkerError::new(
                "invalid_pattern",
                "Invalid or overly complex search pattern; use literal mode for exact text",
            )
        })
}

pub(crate) async fn execute(name: &str, args: &Value, ctx: &ToolContext) -> Result<Value> {
    let path = args["path"].as_str().unwrap_or(".");
    let finding = name == "files.find";
    let recursive = args["recursive"].as_bool().unwrap_or(true);
    let include = pattern(
        args[if finding { "pattern" } else { "include" }]
            .as_str()
            .unwrap_or("**"),
        true,
        false,
    )?;
    let query = args["query"].as_str().unwrap_or("");
    let matcher = pattern(
        &if args["regex"].as_bool().unwrap_or(false) {
            query.into()
        } else {
            regex::escape(query)
        },
        false,
        args["case_sensitive"].as_bool() == Some(false),
    )?;
    let (files, scan_limited) = ctx
        .workspace
        .discover(path, recursive, args["hidden"].as_bool().unwrap_or(false))
        .await?;
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    let limit = args["limit"].as_u64().unwrap_or(40).min(100) as usize;
    let context = args["context_lines"].as_u64().unwrap_or(1).min(3) as usize;
    let mut rows = Vec::new();
    let mut seen = 0;
    let mut skipped = 0;
    let mut has_more = false;
    let mut bytes_read = 0;
    let mut scan_limited = scan_limited;
    'files: for file in files {
        if ctx.cancel.is_cancelled() {
            return Err(WorkerError::new("cancelled", "Search cancelled"));
        }
        let relative = file.strip_prefix(&format!("{path}/")).unwrap_or(&file);
        if !include.is_match(relative) {
            continue;
        }
        let candidates = if finding {
            vec![json!(file)]
        } else {
            let Ok(bytes) = ctx.workspace.read(&file, 1_048_576).await else {
                skipped += 1;
                continue;
            };
            bytes_read += bytes.len();
            if bytes_read > 32 * 1024 * 1024 {
                scan_limited = true;
                break;
            }
            if bytes.contains(&0) {
                skipped += 1;
                continue;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                skipped += 1;
                continue;
            };
            let lines: Vec<_> = text.lines().collect();
            let mut found = Vec::new();
            for (i, line) in lines.iter().enumerate() {
                if let Some(m) = matcher.find(line) {
                    let mut start = m.start().saturating_sub(160);
                    while !line.is_char_boundary(start) {
                        start += 1;
                    }
                    let preview = crate::completion::excerpt(&line[start..], 1024);
                    let around: Vec<_> = (i.saturating_sub(context)
                        ..(i + context + 1).min(lines.len()))
                        .filter(|n| *n != i)
                        .map(
                            |n| json!({"line":n+1,"text":crate::completion::excerpt(lines[n],256)}),
                        )
                        .collect();
                    found.push(json!({"path":file,"line":i+1,"text":preview,"preview_start_byte":start,"context":around}));
                    // A single repetitive file cannot allocate an unbounded result.
                    if found.len() > offset.saturating_add(limit).min(10_000) {
                        break;
                    }
                }
            }
            found
        };
        for row in candidates {
            if seen >= 10_000 {
                scan_limited = true;
                break 'files;
            }
            seen += 1;
            if seen <= offset {
                continue;
            }
            if rows.len() == limit
                || (!rows.is_empty()
                    && serde_json::to_vec(&rows)?.len() + serde_json::to_vec(&row)?.len() > 6000)
            {
                has_more = true;
                break 'files;
            }
            rows.push(row);
        }
    }
    let next = has_more.then_some(offset + rows.len());
    Ok(
        json!({if finding {"paths"} else {"matches"}: rows,"offset":offset,"next_offset":next,
        "has_more":has_more,"scan_limited":scan_limited,"skipped_files":skipped,"recursive":recursive,
        "hint":"Follow next_offset with the same query; narrow path/include if scan_limited. Results reflect current files. Read matched line ranges before editing."}),
    )
}
