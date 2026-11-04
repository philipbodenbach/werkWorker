//! Scoped progress reconstructed from trusted browser receipts, never model claims.
use crate::core::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn is_browser(call: &ToolCall) -> bool {
    matches!(
        call.name.as_str(),
        "browser.observe" | "browser.action" | "browser.navigate"
    )
}

fn scope(data: &Value) -> Option<(String, u64)> {
    Some((
        data["browser_id"].as_str()?.into(),
        data["tab_id"].as_u64()?,
    ))
}

// Resolve ephemeral refs against the observation available BEFORE each call.
// New refs for the same observed control still count as repetitions; different
// controls must not collapse into one generic "ref click". Unknown refs share
// a bounded fallback so invented/stale refs cannot evade the guard.
fn resolved_key(call: &ToolCall, observation: Option<&Value>) -> String {
    let Some(reference) = call.arguments["target"]["ref"].as_str() else {
        return crate::working_state::call_key(call);
    };
    let mut resolved = call.clone();
    let controls = observation.and_then(|v| v["controls"].as_array());
    let identity = |control: &Value| {
        json!({
            "tag":control["tag"],"role":control["role"],"name":control["name"],
            "type":control["type"],"href":control["href"],"color":control["color"]
        })
    };
    let target = controls.and_then(|controls| {
        let control = controls.iter().find(|c| c["ref"] == reference)?;
        let mut key = identity(control);
        let anonymous = ["name", "href"]
            .iter()
            .all(|field| control[*field].as_str().is_none_or(str::is_empty));
        if anonymous || controls.iter().filter(|c| identity(c) == key).count() > 1 {
            key["bounds"] = control["bounds"].clone();
        }
        Some(fingerprint(&key))
    });
    resolved.arguments["target"]["ref"] = json!(target.unwrap_or_else(|| "unobserved_ref".into()));
    crate::working_state::call_key(&resolved)
}

fn resolved_keys(run: &Run, call: &ToolCall) -> (Vec<String>, String) {
    let mut observations = BTreeMap::new();
    let mut keys = Vec::with_capacity(run.invocations.len());
    let mut current = None;
    for invocation in &run.invocations {
        let browser = invocation.call.arguments["browser_id"]
            .as_str()
            .unwrap_or("");
        let key = resolved_key(&invocation.call, observations.get(browser).copied());
        if invocation.call.id == call.id {
            current = Some(key.clone());
        }
        keys.push(key);
        if let Some(data) = invocation.result.as_ref().map(|r| &r.data)
            && let Some(browser) = data["browser_id"].as_str()
            && data["observation_id"].is_string()
        {
            observations.insert(browser, data);
        }
    }
    let current = current.unwrap_or_else(|| {
        resolved_key(
            call,
            observations
                .get(call.arguments["browser_id"].as_str().unwrap_or(""))
                .copied(),
        )
    });
    (keys, current)
}

/// Each browser/tab has its own epoch. Seen semantic states cannot renew it
/// twice (e.g. blinking). Visual novelty has a finite allowance, even if an
/// animation produces infinitely many different frames.
pub(crate) fn repetitions(run: &Run, call: &ToolCall) -> usize {
    let (keys, key) = resolved_keys(run, call);
    let browser = call.arguments["browser_id"].as_str().unwrap_or("");
    let current_scope = run
        .invocations
        .iter()
        .rev()
        .filter_map(|i| i.result.as_ref())
        .filter_map(|r| scope(&r.data))
        .find(|(id, _)| id == browser);
    let Some(current_scope) = current_scope else {
        return keys.iter().filter(|candidate| **candidate == key).count();
    };
    let mut seen = BTreeSet::new();
    let mut frames = BTreeSet::new();
    let mut boundary = 0;
    for (index, invocation) in run.invocations.iter().enumerate() {
        let Some(r) = &invocation.result else {
            continue;
        };
        if scope(&r.data).as_ref() != Some(&current_scope) || r.status != Outcome::Succeeded {
            continue;
        }
        let progress = &r.data["progress"];
        let dom = progress["semantic"]
            .as_str()
            .is_some_and(|s| seen.insert(s.to_owned()));
        let visual = progress["visual"]
            .as_str()
            .is_some_and(|s| frames.len() < 12 && frames.insert(s.to_owned()));
        if dom || visual {
            boundary = index;
        }
    }
    run.invocations[boundary..]
        .iter()
        .zip(&keys[boundary..])
        .filter(|(i, candidate)| {
            **candidate == key
                && i.result
                    .as_ref()
                    .and_then(|r| scope(&r.data))
                    .is_none_or(|s| s == current_scope)
        })
        .count()
}

pub(crate) fn counts(run: &Run) -> BTreeMap<String, usize> {
    let calls: BTreeMap<_, _> = run
        .invocations
        .iter()
        .filter(|i| is_browser(&i.call))
        .map(|i| (crate::working_state::call_key(&i.call), &i.call))
        .collect();
    calls
        .into_iter()
        .map(|(key, call)| (key, repetitions(run, call)))
        .collect()
}

/// Driver supplies a temporally stable, redacted semantic sample. Metadata,
/// references, pointer positions and the screenshot bytes are not DOM progress.
pub(crate) fn stamp(data: &mut Value, bytes: Option<&[u8]>) {
    let semantic = data
        .get("semantic_state")
        .filter(|v| !v.is_null())
        .map(fingerprint);
    let visual = if data["visual_surface"] == true {
        bytes
            .and_then(|b| image::load_from_memory(b).ok())
            .map(|image| {
                // Coarse luminance grid suppresses cursor/subpixel/compression churn.
                let pixels = image
                    .resize_exact(16, 12, image::imageops::FilterType::Triangle)
                    .to_luma8();
                fingerprint(&pixels.into_raw().iter().map(|v| v / 32).collect::<Vec<_>>())
            })
    } else {
        None
    };
    data.as_object_mut().unwrap().remove("semantic_state");
    data["progress"] = json!({"semantic":semantic,"visual":visual});
}

/// Select before counting, compaction and fingerprinting. Original user images
/// and non-browser tool images retain their original semantics and lifetime.
pub(crate) fn select_images(messages: &mut [Message], browser_calls: &BTreeSet<String>) {
    let mut frames = 0;
    for message in messages.iter_mut().rev() {
        if message.role == Role::Tool
            && message
                .tool_call_id
                .as_ref()
                .is_some_and(|id| browser_calls.contains(id))
            && !message.images.is_empty()
        {
            frames += 1;
            if frames > 2 {
                message.images.clear();
            }
        }
    }
}

/// Browser receipts keep the same actionable controls with less default-value
/// noise. Host policy and the browser panel always use the canonical receipt.
pub(crate) fn compact_controls(messages: &mut [Message], browser_calls: &BTreeSet<String>) {
    for message in messages {
        if message.role != Role::Tool
            || !message
                .tool_call_id
                .as_ref()
                .is_some_and(|id| browser_calls.contains(id))
        {
            continue;
        }
        let Ok(mut result) = serde_json::from_str::<Value>(&message.content) else {
            continue;
        };
        let Some(controls) = result["data"]["controls"].as_array_mut() else {
            continue;
        };
        for control in controls {
            if let Some(fields) = control.as_object_mut() {
                for field in [
                    "id",
                    "type",
                    "role",
                    "name",
                    "autocomplete",
                    "href",
                    "form_action",
                ] {
                    if fields.get(field).is_some_and(|v| v == "") {
                        fields.remove(field);
                    }
                }
                for field in [
                    "disabled",
                    "checked",
                    "download",
                    "submits",
                    "protected",
                    "consequential",
                ] {
                    if field == "checked"
                        && ["role", "type"].iter().any(|key| {
                            fields
                                .get(*key)
                                .and_then(Value::as_str)
                                .is_some_and(|kind| {
                                    matches!(
                                        kind,
                                        "checkbox"
                                            | "radio"
                                            | "switch"
                                            | "menuitemcheckbox"
                                            | "menuitemradio"
                                    )
                                })
                        })
                    {
                        continue;
                    }
                    if fields.get(field).is_some_and(|v| v == false) {
                        fields.remove(field);
                    }
                }
            }
        }
        message.content = result.to_string();
    }
}

/// Vision decisions need the current controls, not accumulated expired refs.
/// Project only request-local copies; canonical receipts, page text, accessibility,
/// action outcomes and errors remain intact. DOM research history is unchanged.
pub(crate) fn prune_expired_controls(messages: &mut [Message], browser_calls: &BTreeSet<String>) {
    let mut observed = BTreeSet::new();
    for message in messages.iter_mut().rev() {
        if message.role != Role::Tool
            || !message
                .tool_call_id
                .as_ref()
                .is_some_and(|id| browser_calls.contains(id))
        {
            continue;
        }
        let Ok(mut result) = serde_json::from_str::<Value>(&message.content) else {
            continue;
        };
        let succeeded = result["status"] == "succeeded";
        let Some(data) = result["data"].as_object_mut() else {
            continue;
        };
        if !data.get("observation_id").is_some_and(Value::is_string) {
            continue;
        }
        let Some(browser) = data.get("browser_id").and_then(Value::as_str) else {
            continue;
        };
        let latest = observed.insert(browser.to_owned());
        if latest || !succeeded || data.get("perception").and_then(Value::as_str) != Some("vision")
        {
            continue;
        }
        if let Some(Value::Array(controls)) = data.remove("controls") {
            if !controls.is_empty() {
                data.insert("controls_omitted".into(), json!({"count":controls.len(),"reason":"References expired after a newer observation; use the latest controls."}));
            }
            message.content = result.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_defaults_do_not_hide_names_bounds_or_policy_flags() {
        let mut result = ToolResult::ok(json!({"controls":[
            {"ref":"e1","tag":"canvas","name":"","href":"","protected":false,"bounds":{"x":0,"y":25}},
            {"ref":"e2","name":"Password","protected":true,"consequential":true,"disabled":true,"checked":true},
            {"ref":"e3","role":"checkbox","checked":false},
            {"ref":"e4","role":"menuitemcheckbox","checked":false}
        ],"error":"Click failed","action_outcome":"unknown"}));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        result
            .images
            .push(crate::attachments::ImageAttachment::from_bytes(bytes.get_ref()).unwrap());
        let original = Message::tool("browser", &result);
        let mut messages = vec![original.clone()];
        compact_controls(&mut messages, &["browser".into()].into());
        let projected: Value = serde_json::from_str(&messages[0].content).unwrap();
        assert_eq!(
            projected["data"]["controls"][0],
            json!({"ref":"e1","tag":"canvas","bounds":{"x":0,"y":25}})
        );
        assert_eq!(projected["data"]["controls"][1], result.data["controls"][1]);
        assert_eq!(projected["data"]["controls"][2], result.data["controls"][2]);
        assert_eq!(projected["data"]["controls"][3], result.data["controls"][3]);
        assert_eq!(projected["data"]["error"], result.data["error"]);
        assert_eq!(messages[0].images, original.images);
        assert!(original.content.contains("\"protected\":false"));
        // screenshot:false and DOM-only observations need the same projection.
        let mut without_image = original.clone();
        without_image.images.clear();
        let mut text_only = vec![without_image];
        compact_controls(&mut text_only, &["browser".into()].into());
        assert_eq!(text_only[0].content, messages[0].content);
        assert!(text_only[0].images.is_empty());
        let projected = text_only.clone();
        compact_controls(&mut text_only, &["browser".into()].into());
        assert_eq!(text_only, projected);
    }

    #[test]
    fn vision_history_discards_only_expired_controls() {
        let receipt = |id: &str, browser: &str, perception: &str| {
            Message::tool(
                id,
                &ToolResult::ok(
                    json!({"browser_id":browser,"observation_id":id,"perception":perception,
                "controls":[{"ref":id,"bounds":{"x":10,"y":20,"width":100,"height":100}}],
                "text":format!("Page text at {id}"),"accessibility":"Historical accessibility", "action_executed":true}),
                ),
            )
        };
        let original = vec![
            receipt("old", "b1", "vision"),
            receipt("other", "b2", "vision"),
            receipt("dom", "b1", "dom"),
            receipt("new", "b1", "vision"),
            receipt("file", "b1", "vision"),
        ];
        let mut messages = original.clone();
        let calls = ["old", "other", "dom", "new"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        prune_expired_controls(&mut messages, &calls);
        let old: Value = serde_json::from_str(&messages[0].content).unwrap();
        assert!(old["data"]["controls"].is_null());
        assert_eq!(old["data"]["controls_omitted"]["count"], 1);
        assert_eq!(old["data"]["text"], "Page text at old");
        assert_eq!(old["data"]["accessibility"], "Historical accessibility");
        assert_eq!(old["data"]["action_executed"], true);
        for n in 1..messages.len() {
            assert_eq!(messages[n].content, original[n].content);
        }
        // Projection is idempotent and does not rewrite the durable source.
        let once = messages.clone();
        prune_expired_controls(&mut messages, &calls);
        assert_eq!(fingerprint(&messages), fingerprint(&once));
        assert!(original[0].content.contains("\"controls\""));
    }

    #[test]
    fn observation_failure_expires_old_refs_without_hiding_failure_evidence() {
        let old = Message::tool(
            "old",
            &ToolResult::ok(
                json!({"browser_id":"b","observation_id":"one","perception":"vision","controls":[{"ref":"e1"}]}),
            ),
        );
        let mut error = ToolResult::error(Outcome::Failed, "browser_action_failed");
        error.data = json!({"browser_id":"b","observation_id":"two","perception":"vision","action_executed":true,"observation_errors":[{"part":"protection","reason":"masking_unavailable"}]});
        let failed = Message::tool("failed", &error);
        let mut messages = vec![old, failed.clone()];
        let calls = ["old", "failed"].into_iter().map(str::to_owned).collect();
        prune_expired_controls(&mut messages, &calls);
        assert!(
            serde_json::from_str::<Value>(&messages[0].content).unwrap()["data"]["controls"]
                .is_null()
        );
        assert_eq!(messages[1].content, failed.content);
    }

    #[test]
    #[ignore = "read-only saved browser history: WW_REPLAY_DATABASE and WW_REPLAY_SESSION"]
    fn saved_vision_controls_projection() {
        let db = rusqlite::Connection::open_with_flags(
            std::env::var("WW_REPLAY_DATABASE").unwrap(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let raw: String = db
            .query_row(
                "SELECT data FROM sessions WHERE id = ?1",
                [std::env::var("WW_REPLAY_SESSION").unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        let session: SessionData = serde_json::from_str(&raw).unwrap();
        let calls = session
            .runs
            .iter()
            .flat_map(|r| &r.invocations)
            .filter(|i| crate::browser_policy::TOOLS.contains(&i.call.name.as_str()))
            .map(|i| i.call.id.clone())
            .collect();
        let mut messages = session.messages.clone();
        let before: usize = messages.iter().map(|m| m.content.len()).sum();
        prune_expired_controls(&mut messages, &calls);
        let after: usize = messages.iter().map(|m| m.content.len()).sum();
        eprintln!(
            "Saved vision history text: {before} -> {after} bytes ({} removed); original database untouched",
            before.saturating_sub(after)
        );
        assert!(after < before);
        assert_eq!(
            messages.iter().map(|m| m.images.len()).sum::<usize>(),
            session
                .messages
                .iter()
                .map(|m| m.images.len())
                .sum::<usize>()
        );
    }
    fn run() -> Run {
        serde_json::from_value(json!({"id":"r","status":"running","input":"Browse", "limits":Limits::default(),"started_at_ms":0,"deadline_ms":0,"history_start":0,"metrics":Metrics::default(),"attempts":[],"contexts":[],"invocations":[],"steering":[],"plan":null,"similar_grants":[],"repaired_output":false})).unwrap()
    }
    #[test]
    fn ref_clicks_distinguish_controls_but_still_stop_repeated_selection() {
        let mut run = run();
        let observation = |n: usize| {
            ToolResult::ok(json!({
                "browser_id":"browser","tab_id":1,"observation_id":format!("o{n}"),
                "progress":{"semantic":"unchanged"},
                "controls":[
                    {"ref":format!("fill{n}"),"tag":"div","name":"Fill","bounds":{"x":20+n,"y":40}},
                    {"ref":format!("blue{n}"),"tag":"div","color":"rgb(0,0,255)","bounds":{"x":50,"y":700}}
                ]
            }))
        };
        run.invocations.push(serde_json::from_value(json!({"invocation_id":"open","step_id":"s","message_index":0,
            "call":{"id":"open","name":"browser.open","arguments":{}},"status":"finished","prepared":{},"result":observation(0)})).unwrap());
        for (n, target) in ["fill", "blue", "fill", "fill", "fill"]
            .into_iter()
            .enumerate()
        {
            let call = ToolCall {
                id: format!("c{n}"),
                name: "browser.action".into(),
                arguments: json!({"browser_id":"browser","action":"click","target":{"ref":format!("{target}{n}"),"name":format!("explanation {n}")}}),
            };
            run.invocations.push(
                serde_json::from_value(
                    json!({"invocation_id":format!("i{n}"),"step_id":"s","message_index":n+1,
                "call":call,"status":"finished","prepared":{},"result":observation(n+1)}),
                )
                .unwrap(),
            );
            let expected = [1, 1, 2, 3, 4][n];
            assert_eq!(super::repetitions(&run, &call), expected);
            assert_eq!(
                crate::harness::repetition_counts(&run)[&crate::working_state::call_key(&call)],
                expected
            );
        }
        let restored: Run = serde_json::from_value(serde_json::to_value(&run).unwrap()).unwrap();
        assert_eq!(
            super::repetitions(&restored, &run.invocations.last().unwrap().call),
            4
        );
        // Invalid refs cannot reset the guard just by inventing another ID.
        for n in 0..5 {
            let call = ToolCall {
                id: format!("invalid{n}"),
                name: "browser.action".into(),
                arguments: json!({"browser_id":"browser","action":"click","target":{"ref":format!("invented{n}")}}),
            };
            run.invocations.push(serde_json::from_value(json!({"invocation_id":format!("bad{n}"),"step_id":"s","message_index":100+n,
                "call":call,"status":"finished","prepared":{},"result":ToolResult::error(Outcome::ValidationError,"invalid_browser_target")})).unwrap());
            assert_eq!(super::repetitions(&run, &call), n + 1);
        }
    }
    fn add(
        run: &mut Run,
        browser: &str,
        tab: u64,
        semantic: &str,
        visual: Option<String>,
    ) -> ToolCall {
        let n = run.invocations.len();
        let call = ToolCall {
            id: format!("c{n}"),
            name: "browser.observe".into(),
            arguments: json!({"browser_id":browser}),
        };
        run.invocations.push(serde_json::from_value(json!({"invocation_id":format!("i{n}"),"step_id":"s","message_index":n,"call":call,"status":"finished","prepared":{},"result":ToolResult::ok(json!({"browser_id":browser,"tab_id":tab,"revision":n,"progress":{"semantic":semantic,"visual":visual}}))})).unwrap());
        call
    }
    #[test]
    fn changed_states_progress_but_blinking_other_sessions_and_animation_are_bounded() {
        let mut run = run();
        for n in 0..8 {
            let call = add(&mut run, "browser", 1, &format!("state{n}"), None);
            assert_eq!(super::repetitions(&run, &call), 1);
            assert_eq!(
                crate::harness::repetition_counts(&run)[&crate::working_state::call_key(&call)],
                1
            );
        }
        for n in 0..8 {
            add(&mut run, "other", 2, &format!("foreign{n}"), None);
            add(&mut run, "browser", 2, &format!("unrelated_tab{n}"), None);
            let call = add(&mut run, "browser", 1, &format!("state{}", n % 2), None);
            assert!(super::repetitions(&run, &call) >= n + 2);
        }
        for n in 0..24 {
            add(&mut run, "browser", 1, "state0", Some(format!("frame{n}")));
        }
        let call = run.invocations.last().unwrap().call.clone();
        assert!(super::repetitions(&run, &call) > 3);
        let restored: Run = serde_json::from_value(serde_json::to_value(&run).unwrap()).unwrap();
        assert_eq!(
            super::repetitions(&run, &call),
            super::repetitions(&restored, &call)
        );
    }
    #[test]
    fn browser_image_selection_preserves_user_and_other_tool_images() {
        let image = crate::attachments::ImageAttachment::from_bytes(&{
            let mut bytes = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(16, 16)
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            bytes.into_inner()
        })
        .unwrap();
        let mut user = Message::text(Role::User, "reference");
        user.images.push(image.clone());
        let mut messages = vec![user];
        let mut calls = BTreeSet::new();
        for n in 0..20 {
            let id = format!("c{n}");
            calls.insert(id.clone());
            let mut result = ToolResult::ok(json!({}));
            result.images.push(image.clone());
            messages.push(Message::tool(&id, &result));
        }
        let mut other = Message::text(Role::Tool, "other image task");
        other.images.push(image);
        messages.push(other);
        select_images(&mut messages, &calls);
        assert_eq!(messages.iter().map(|m| m.images.len()).sum::<usize>(), 4);
        assert_eq!(messages[0].images.len(), 1);
        assert_eq!(messages[19].images.len(), 1);
        assert_eq!(messages[20].images.len(), 1);
    }
}
