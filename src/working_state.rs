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
    if crate::browser_progress::is_browser(call)
        && let Some(args) = args.as_object_mut()
    {
        args.remove("screenshot");
        args.remove("visual");
        if let Some(target) = args.get_mut("target").and_then(Value::as_object_mut)
            && target.get("ref").is_some_and(Value::is_string)
        {
            target.remove("name");
        }
    }
    for field in ["path", "cwd"] {
        if let Some(value) = args[field].as_str().and_then(|p| path(p).ok()) {
            args[field] = json!(value);
        }
    }
    fingerprint(&(&call.name, args))
}

pub(crate) fn latest_browser_observation<'a>(
    session: &'a SessionData,
    run: &'a Run,
) -> Option<&'a ToolInvocation> {
    ordered(session, run)
        .filter(|i| crate::browser_policy::TOOLS.contains(&i.call.name.as_str()))
        .filter(|i| {
            i.result.as_ref().is_some_and(|r| {
                r.data["browser_id"].is_string() && r.data["observation_id"].is_string()
            })
        })
        .last()
}

/// Browser receipts complement file evidence; plan text remains intent only.
pub(crate) fn browser_checkpoint(session: &SessionData, run: &Run) -> Option<Message> {
    let invocation = latest_browser_observation(session, run)?;
    let data = &invocation.result.as_ref()?.data;
    let observation: serde_json::Map<String, Value> = [
        "browser_id",
        "tab_id",
        "url",
        "viewport",
        "observation_id",
        "revision",
        "observed_at_ms",
        "screenshot",
        "progress",
        "observation_errors",
        "perception",
        "action_executed",
    ]
    .into_iter()
    .filter_map(|key| data.get(key).map(|v| (key.into(), v.clone())))
    .collect();
    let last_action = ordered(session, run)
        .filter(|i| {
            matches!(
                i.call.name.as_str(),
                "browser.open" | "browser.navigate" | "browser.action" | "browser.evaluate"
            ) && i.result.as_ref().is_some_and(|r| {
                r.data["browser_id"] == data["browser_id"]
                    && (r.data["action_executed"] == true || r.data["action_outcome"] == "unknown")
            })
        })
        .last();
    Some(crate::harness::feedback(
        "browser_checkpoint",
        json!({
            "intent":crate::completion::excerpt(&run.plan.to_string(),1024),"intent_trust":"model plan, not evidence",
            "last_action":last_action.map(|i|json!({"tool":i.call.name,"action":i.call.arguments["action"]})),
            "observed_effect":{"title":data["title"],"text_excerpt":crate::completion::excerpt(data["text"].as_str().unwrap_or(""),768),"status":invocation.result.as_ref().map(|r|&r.status)},
            "observation":observation,
            "uncertainty":"Historical receipt, not a live-session assertion. Use the retained observation and attached frame. Observe if missing or stale, after takeover or browser_changed. After restart open a new browser if the session is missing.",
            "next_step":"Compare the latest observation with the user goal; only claim criteria actually observed."
        }),
    ))
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
            "files.read" | "files.write" | "files.edit" | "files.insert"
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
            for field in ["total_lines", "next_insert_after_line"] {
                if let Some(value) = r.data.get(field) {
                    receipt[field] = value.clone();
                }
            }
            if i.call.name == "files.read" {
                if r.data["mode"] == "bytes" {
                    for field in [
                        "mode",
                        "start_byte",
                        "end_byte",
                        "next_start_byte",
                        "total_bytes",
                    ] {
                        if let Some(value) = r.data.get(field) {
                            receipt[field] = value.clone();
                        }
                    }
                } else {
                    receipt["lines"] = json!([r.data["start_line"], r.data["end_line"]]);
                    receipt["total_lines"] = r.data["total_lines"].clone();
                    receipt["next_start_line"] = r.data["next_start_line"].clone();
                }
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
        if !matches!(
            i.call.name.as_str(),
            "files.write" | "files.edit" | "files.insert"
        ) {
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
            // Legacy receipts advertised progressively smaller future limits.
            // Keep the failure evidence, but never present that obsolete
            // instruction as the current contract after reopening a session.
            let detail = if r.data["max_new_characters"].as_u64().is_some_and(|cap| {
                r.data["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("Next repair"))
                    || r.data["allowed_characters"]
                        .as_u64()
                        .is_none_or(|limit| cap < limit)
            }) {
                json!(format!(
                    "Earlier size rejection, no call executed. Argument character counts: {}. The old shrinking repair limit is obsolete; follow the current tool schema. Preserve the existing file and correct one complete edit.",
                    r.data["argument_characters"]
                ))
            } else {
                r.data["message"].clone()
            };
            repairs.insert(
                name.clone(),
                json!({"path":name,"error":r.error_code,"attempts":attempts,
                "detail":detail}),
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
    for tool in tools {
        if !repairs.is_empty()
            && matches!(
                tool.name.as_str(),
                "files.write" | "files.edit" | "files.insert"
            )
        {
            tool.description.push_str(" Pending repair: follow this tool's current schema limits. Earlier rejections do not shrink them. Preserve existing files and correct one complete edit, leaving room below maxLength for copied anchor text.");
        }
        if crate::completion::coding(session)
            && !repairs.is_empty()
            && matches!(tool.name.as_str(), "process.run" | "process.start")
        {
            tool.description.push_str(" Pending file repair: shell workspace is read-only until a corrected file-tool change succeeds. Preserve existing files; do not delete/rebuild them to work around a rejected edit.");
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

#[cfg(test)]
mod browser_tests {
    use super::*;
    use crate::{api::WorkerEngine, policy::HostPolicy, profiles::AgentProfile, providers::*};
    use std::sync::Arc;

    #[tokio::test]
    async fn validation_is_not_an_observation_and_real_checkpoint_echo_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let engine =
            WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
                "seed",
            )])))
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                AgentProfile {
                    schema_version: 1,
                    name: "fixture".into(),
                    prompt: "Browse".into(),
                    tools: vec![],
                    skills: vec![],
                },
                dir.path(),
                HostPolicy::default(),
                Value::Null,
            )
            .await
            .unwrap();
        session
            .submit("Browse", "seed", Limits::default(), None)
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        let mut data = session.snapshot().await.unwrap();
        engine.close().await.unwrap();
        let invalid: ToolInvocation = serde_json::from_value(json!({
            "invocation_id":"invalid", "step_id":"step", "message_index":1,
            "call":{"id":"invalid","name":"browser.open","arguments":{}},
            "status":"finished","prepared":{},
            "result":ToolResult::error(Outcome::ValidationError,"validation_error")
        }))
        .unwrap();
        data.runs[0].invocations.push(invalid.clone());
        assert!(browser_checkpoint(&data, &data.runs[0]).is_none());

        let observed = &mut data.runs[0].invocations[0];
        observed.invocation_id = "opened".into();
        observed.call.id = "opened".into();
        observed.call.arguments = json!({"url":"https://example.com"});
        observed.result = Some(ToolResult::ok(json!({
            "browser_id":"live", "observation_id":"observed-1", "tab_id":1,
            "action_executed":true, "title":"Actual page"
        })));
        data.runs[0].invocations.push(invalid);
        let checkpoint = browser_checkpoint(&data, &data.runs[0]).unwrap();
        let value: Value = serde_json::from_str(&checkpoint.content).unwrap();
        assert_eq!(value["data"]["observation"]["observation_id"], "observed-1");
        assert_eq!(value["data"]["observed_effect"]["status"], "succeeded");
        let mut request = ModelRequest {
            messages: vec![checkpoint.clone()],
            tools: vec![],
            max_output_tokens: 512,
            output_schema: None,
            reasoning_tier: None,
        };
        assert!(
            crate::harness::is_feedback_echo(&data, &data.runs[0], &request, &checkpoint).unwrap()
        );
        request.messages.clear();
        assert!(
            !crate::harness::is_feedback_echo(&data, &data.runs[0], &request, &checkpoint).unwrap(),
            "A harness source label alone does not establish ownership"
        );
    }
}
