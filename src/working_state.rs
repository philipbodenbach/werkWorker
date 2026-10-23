//! Deterministic working state from tool receipts, independent of compaction.
use crate::{core::*, runtime::Workspace};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn path(value: &str) -> Result<String> {
    let value = crate::policy::workspace_path(value)?;
    let normalized = std::path::Path::new(value)
        .components()
        .filter(|p| !matches!(p, std::path::Component::CurDir))
        .map(|p| p.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Ok(if normalized.is_empty() {
        ".".into()
    } else {
        normalized
    })
}

pub(crate) fn call_key(call: &ToolCall) -> String {
    let mut args = call.arguments.clone();
    for field in ["path", "cwd"] {
        if let Some(value) = args[field].as_str().and_then(|p| path(p).ok()) {
            args[field] = json!(value);
        }
    }
    fingerprint(&(&call.name, args))
}

fn ordered<'a>(session: &'a SessionData, run: &'a Run) -> impl Iterator<Item = &'a ToolInvocation> {
    session
        .runs
        .iter()
        .take_while(move |r| r.id != run.id)
        .chain(std::iter::once(run))
        .flat_map(|r| &r.invocations)
}

/// Last observation wins, including absence. Listings only invalidate visible
/// immediate children; a partial listing cannot establish absence.
pub(crate) fn files(session: &SessionData, run: &Run) -> Vec<Value> {
    let mut files: BTreeMap<String, (usize, Value)> = BTreeMap::new();
    for (seq, i) in ordered(session, run).enumerate() {
        let Some(r) = &i.result else { continue };
        let stored = session.metadata["_werkworker_harness"]
            .as_object()
            .and_then(|runs| {
                runs.values()
                    .find_map(|state| state["workspace_changes"].get(&i.invocation_id))
            });
        if i.call.name == "process.run"
            && let Some(changes) = stored.unwrap_or(&r.data["workspace_changes"]).as_object()
        {
            for (name, observation) in changes {
                if let Ok(name) = path(name) {
                    let mut receipt = observation.clone();
                    receipt["path"] = json!(name);
                    receipt["tool"] = json!(i.call.name);
                    files.insert(name, (seq, receipt));
                }
            }
        }
        let Some(name) = i.call.arguments["path"].as_str().and_then(|p| path(p).ok()) else {
            continue;
        };
        if i.call.name == "files.list" && r.status == Outcome::Succeeded && !r.truncated {
            if let Some(entries) = r.data["entries"].as_array() {
                for (file, (order, receipt)) in &mut files {
                    let file = std::path::Path::new(file);
                    let parent = file.parent().and_then(|p| p.to_str()).unwrap_or("");
                    let parent = if parent.is_empty() { "." } else { parent };
                    let base = file.file_name().and_then(|p| p.to_str()).unwrap_or("");
                    if parent == name
                        && !base.starts_with('.')
                        && !entries.iter().any(|v| v == base)
                    {
                        *order = seq;
                        *receipt = json!({"path":file,"status":"missing","tool":"files.list"});
                    }
                }
            }
            continue;
        }
        if !matches!(
            i.call.name.as_str(),
            "files.read" | "files.write" | "files.edit"
        ) {
            continue;
        }
        let mut receipt = json!({"path":name,"tool":i.call.name});
        if matches!(
            r.error_code.as_deref(),
            Some("not_found" | "file_not_found")
        ) {
            receipt["status"] = json!("missing");
        } else if r.status == Outcome::Succeeded && r.data["sha256"].is_string() {
            receipt["status"] = json!("present");
            receipt["sha256"] = r.data["sha256"].clone();
            if i.call.name == "files.read" {
                receipt["lines"] = json!([r.data["start_line"], r.data["end_line"]]);
                receipt["total_lines"] = r.data["total_lines"].clone();
                receipt["next_start_line"] = r.data["next_start_line"].clone();
                if let Some(content) = r.data["content"].as_str() {
                    let mut start = content.len().saturating_sub(384);
                    while !content.is_char_boundary(start) {
                        start += 1;
                    }
                    receipt["observed_tail_excerpt"] = json!(&content[start..]);
                }
            }
        } else {
            continue;
        }
        files.insert(name, (seq, receipt));
    }
    let mut values: Vec<_> = files.into_values().collect();
    values.sort_by_key(|(seq, _)| std::cmp::Reverse(*seq));
    values.into_iter().map(|(_, v)| v).collect()
}

pub(crate) fn bounded_files(session: &SessionData, run: &Run) -> Vec<Value> {
    let mut selected = Vec::new();
    let mut size = 2;
    for value in files(session, run).into_iter().take(8) {
        size += value.to_string().len() + 1;
        if size > 4096 {
            break;
        }
        selected.push(value);
    }
    selected
}

/// Pending repairs survive reads, plans, compaction and resume. A new user
/// instruction or a successful change of the affected file supersedes them.
pub(crate) fn repairs(session: &SessionData, run: &Run) -> Vec<Value> {
    if crate::harness::config(session, run)
        .is_ok_and(|c| c.mode == crate::harness::HarnessMode::Classic)
    {
        return Vec::new();
    }
    let user_boundary = session
        .messages
        .iter()
        .rposition(|m| m.role == Role::User)
        .unwrap_or(run.history_start);
    let mut repairs: BTreeMap<String, Value> = BTreeMap::new();
    for i in &run.invocations {
        if i.message_index < user_boundary {
            continue;
        }
        if !matches!(i.call.name.as_str(), "files.write" | "files.edit") {
            continue;
        }
        let Some(r) = &i.result else { continue };
        let Some(name) = i.call.arguments["path"].as_str().and_then(|p| path(p).ok()) else {
            continue;
        };
        if r.status == Outcome::Succeeded
            && r.data["sha256"].is_string()
            && r.data["sha256"] != i.call.arguments["expected_hash"]
        {
            repairs.remove(&name);
        } else if matches!(
            r.error_code.as_deref(),
            Some("write_conflict" | "recovery_step_too_large" | "coding_step_invalid")
        ) {
            let attempts = repairs
                .get(&name)
                .and_then(|v| v["attempts"].as_u64())
                .unwrap_or(0)
                + 1;
            let previous = repairs.get(&name);
            let cap = r.data["max_new_characters"]
                .as_u64()
                .or_else(|| previous.and_then(|v| v["max_new_characters"].as_u64()))
                .unwrap_or(2048);
            let size_limited = r.data["max_new_characters"].is_u64()
                || previous.is_some_and(|v| v["size_limited"] == true);
            repairs.insert(
                name.clone(),
                json!({"path":name,"error":r.error_code,"attempts":attempts,
                "max_new_characters":cap,"size_limited":size_limited,"detail":r.data["message"]}),
            );
        }
    }
    repairs.into_values().collect()
}

pub(crate) fn protect_process(session: &SessionData, run: &Run) -> bool {
    crate::completion::coding(session) && !repairs(session, run).is_empty()
}

pub(crate) fn bound_repairs(session: &SessionData, run: &Run, tools: &mut [ToolDefinition]) {
    let repairs = repairs(session, run);
    let cap = repairs
        .iter()
        .filter(|r| r["size_limited"] == true)
        .filter_map(|r| r["max_new_characters"].as_u64())
        .min();
    for tool in tools {
        let fields: &[&str] = match tool.name.as_str() {
            "files.write" => &["content"],
            "files.edit" => &["new"],
            _ => &[],
        };
        if let Some(cap) = cap {
            for field in fields {
                let property = &mut tool.input_schema["properties"][field];
                property["maxLength"] =
                    json!(property["maxLength"].as_u64().unwrap_or(cap).min(cap));
            }
            if !fields.is_empty() {
                tool.description.push_str(&format!(" Pending repair: at most {cap} characters of new content. Complete one function/block; never truncate code to fit."));
            }
        }
        if crate::completion::coding(session)
            && !repairs.is_empty()
            && matches!(tool.name.as_str(), "process.run" | "process.start")
        {
            tool.description.push_str(" Pending file repair: shell workspace is read-only until a corrected files.edit/files.write succeeds. Preserve existing files; do not delete/rebuild them to work around a rejected edit.");
        }
    }
}

/// Observe only known paths, with bounded I/O. No recursive repository scan.
pub(crate) async fn capture(
    session: &SessionData,
    run: &Run,
    workspace: &Workspace,
) -> BTreeMap<String, Value> {
    let names: BTreeSet<_> = files(session, run)
        .into_iter()
        .take(32)
        .filter_map(|v| v["path"].as_str().map(str::to_owned))
        .collect();
    let mut result = BTreeMap::new();
    for name in names {
        let observation = match workspace.read(&name, 1_048_576).await {
            Ok(bytes) => json!({"status":"present","sha256":hash(bytes)}),
            Err(e) if e.code == "not_found" => json!({"status":"missing"}),
            Err(_) => json!({"status":"unverified"}),
        };
        result.insert(name, observation);
    }
    result
}

pub(crate) fn changes(before: &BTreeMap<String, Value>, after: BTreeMap<String, Value>) -> Value {
    json!(
        after
            .into_iter()
            .filter(|(p, value)| before.get(p) != Some(value))
            .collect::<BTreeMap<_, _>>()
    )
}
