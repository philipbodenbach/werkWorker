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

/// Rebuild an actionable warning from durable receipts on every decision.
/// Unlike a one-shot adaptive hint, this survives compaction and follows both
/// repeated calls and cycles across different actions. Never grants extra calls.
pub(crate) fn feedback(session: &SessionData, run: &Run) -> Result<Option<Message>> {
    if let Some(strategy) = strategy_feedback(run) {
        return Ok(Some(strategy));
    }
    repetition_feedback(session, run)
}

/// Different calls can cycle through the same observed states. This is evidence
/// to reconsider the method, not permission to reset a guard or claim failure of
/// an application operation. Unknown outcomes require observation first.
fn strategy_feedback(run: &Run) -> Option<Message> {
    strategy_data(run).map(|data| crate::harness::feedback("browser_strategy", data))
}

fn strategy_data(run: &Run) -> Option<Value> {
    #[derive(Default)]
    struct Trace<'a> {
        seen: BTreeSet<String>,
        stagnant: Vec<&'a ToolInvocation>,
        observation: Option<&'a Value>,
    }
    let mut traces = BTreeMap::<(String, u64), Trace<'_>>::new();
    let mut tabs = BTreeMap::<String, (String, u64)>::new();
    let mut current = None;
    for invocation in &run.invocations {
        let Some(result) = &invocation.result else {
            continue;
        };
        if !is_browser(&invocation.call) && invocation.call.name != "browser.open" {
            continue;
        }
        if matches!(
            result.status,
            Outcome::ValidationError | Outcome::Denied | Outcome::CancelledBeforeStart
        ) {
            current = invocation.call.arguments["browser_id"]
                .as_str()
                .and_then(|browser| tabs.get(browser))
                .cloned();
            if let Some(key) = &current {
                traces
                    .entry(key.clone())
                    .or_default()
                    .stagnant
                    .push(invocation);
            }
            continue;
        }
        let data = &result.data;
        let next = scope(data);
        let progress = &data["progress"];
        if result.status != Outcome::Succeeded
            || next.is_none()
            || (!progress["semantic"].is_string() && !progress["visual"].is_string())
        {
            if let Some(browser) = invocation.call.arguments["browser_id"].as_str()
                && let Some(key) = tabs.get(browser)
            {
                let trace = traces.entry(key.clone()).or_default();
                trace.stagnant.clear();
                trace.observation = None;
            }
            current = None;
            continue;
        }
        current = next;
        let key = current.clone().unwrap();
        tabs.insert(key.0.clone(), key.clone());
        let trace = traces.entry(key).or_default();
        let state = fingerprint(&(progress["semantic"].as_str(), progress["visual"].as_str()));
        let inspected = invocation
            .request_input_schema
            .as_ref()
            .is_some_and(|schema| schema["x-werk-browser-strategy"] == "inspect")
            && trace.observation.is_some_and(|previous| {
                fingerprint(&(
                    previous["progress"]["semantic"].as_str(),
                    previous["progress"]["visual"].as_str(),
                )) != state
            });
        let novel = trace.seen.insert(state);
        // A verified alternative may reopen a previously seen tool/options
        // state. Release gestures after that inspection; hard guards still use
        // their original histories and are never reset by this transition.
        if novel || inspected {
            trace.stagnant.clear();
        } else if is_browser(&invocation.call) {
            trace.stagnant.push(invocation);
        }
        trace.observation = Some(data);
    }
    // A few setup operations are normal. Escalate the guidance before a long
    // alternating-action loop, independently of a one-shot adaptive hint.
    let trace = traces.get(&current?)?;
    let stagnant = &trace.stagnant;
    if stagnant.len() < run.limits.max_repeated_calls.max(1).saturating_mul(2) {
        return None;
    }
    let data = trace.observation?;
    let controls = data["controls"].as_array()?;
    let attempted: BTreeSet<_> = stagnant
        .iter()
        .filter(|i| i.call.arguments["browser_id"] == data["browser_id"])
        .filter_map(|i| i.result.as_ref())
        .filter(|r| scope(&r.data) == scope(data))
        .filter_map(|r| r.data["action_target"]["name"].as_str())
        .collect();
    let mut names = Vec::new();
    let mut bytes = 0;
    for control in controls {
        if control["disabled"] == true
            || control["selected"] == true
            || control["protected"] == true
            || control["consequential"] == true
            || control["submits"] == true
            || control["download"] == true
        {
            continue;
        }
        let Some(name) = control["name"]
            .as_str()
            .filter(|n| !n.is_empty() && !attempted.contains(n))
        else {
            continue;
        };
        let name = crate::completion::excerpt(name, 96);
        if bytes + name.len() > 1024 || names.len() == 24 {
            break;
        }
        bytes += name.len();
        names.push(name);
    }
    let surfaces: Vec<_> = controls
        .iter()
        .filter(|c| matches!(c["tag"].as_str(), Some("canvas" | "svg")))
        .filter(|c| {
            c["bounds"]["width"].as_f64().unwrap_or(0.)
                * c["bounds"]["height"].as_f64().unwrap_or(0.)
                >= 16384.
        })
        .take(4)
        .map(|c| c["bounds"].clone())
        .collect();
    let recent: Vec<_> = stagnant
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(|i| {
            let result = i.result.as_ref().unwrap();
            let args = &i.call.arguments;
            let mut entry = json!({"action":args["action"],"result":result.status,
            "actual_target":target_summary(&result.data["action_target"])});
            if args["x"].is_number() && args["y"].is_number() {
                entry["point"] = json!({"x":args["x"],"y":args["y"]});
            }
            entry
        })
        .collect();
    Some(json!({
        "browser_id":data["browser_id"],"observation_id":data["observation_id"],"attempts_without_new_state":stagnant.len(),
        "recent_actions":recent,"untried_control_names":names,"visual_surface_bounds":surfaces,
        "detail":"These different attempts returned already-seen page states or were rejected before execution. Change the unsuccessful method now. Inspect an untried native control and its options; do not assume its capabilities without inspecting them. For visual editing, consider shape/fill modes or selections/masks instead of repeating flood fills. A color/tool selection alone does not change the work. Use the actual surface bounds for pointer actions, not the surrounding workspace. Choose one concrete alternative operation and verify its visible effect. Use a fresh target:{ref} alone for a known control. Preserve existing work and all user constraints. Counters and limits are unchanged; this is not completion."
    }))
}

/// Use the engine's existing per-request schema gate: inspect a different
/// available control before issuing more blind coordinates. No actions execute
/// automatically, and a new observed state restores the ordinary catalog.
pub(crate) fn bound_strategy_tools(run: &Run, tools: &mut [ToolDefinition]) {
    let Some(data) = strategy_data(run) else {
        return;
    };
    let Some(observation) = run
        .invocations
        .iter()
        .rev()
        .filter_map(|i| i.result.as_ref())
        .find(|r| {
            r.data["browser_id"] == data["browser_id"]
                && r.data["observation_id"] == data["observation_id"]
        })
    else {
        return;
    };
    let Some(controls) = observation.data["controls"].as_array() else {
        return;
    };
    let names = data["untried_control_names"].as_array().unwrap();
    let references: Vec<_> = controls
        .iter()
        .filter(|c| {
            [
                "disabled",
                "selected",
                "protected",
                "consequential",
                "submits",
                "download",
            ]
            .iter()
            .all(|key| c[*key] != true)
        })
        .filter(|c| {
            c["name"]
                .as_str()
                .is_some_and(|name| names.contains(&json!(crate::completion::excerpt(name, 96))))
        })
        .filter_map(|c| c["ref"].as_str())
        .take(64)
        .collect();
    if references.is_empty() {
        return;
    }
    let Some(tool) = tools.iter_mut().find(|t| t.name == "browser.action") else {
        return;
    };
    let Some(properties) = tool.input_schema["properties"].as_object_mut() else {
        return;
    };
    properties.retain(|key, _| {
        matches!(
            key.as_str(),
            "browser_id" | "action" | "target" | "screenshot" | "visual"
        )
    });
    properties.insert(
        "action".into(),
        json!({"type":"string","enum":["click","hover"]}),
    );
    properties.get_mut("browser_id").unwrap()["enum"] = json!([data["browser_id"]]);
    properties.insert("target".into(),json!({"type":"object","properties":{"ref":{"type":"string","enum":references}},"required":["ref"],"additionalProperties":false}));
    tool.input_schema["required"] = json!(["browser_id", "action", "target"]);
    tool.input_schema["x-werk-browser-strategy"] = json!("inspect");
    tool.description = "Strategy recovery: recent different actions produced no new page state. Inspect an alternative visible control and its options with click or hover using one of the current target.ref values. Choose a method relevant to the user's pending goal; preserve existing work. Coordinate gestures return after new UI evidence. Observe separately only if the current view is missing or stale. Permissions and all run limits still apply.".into();
}

fn target_summary(observed: &Value) -> Value {
    let mut target = serde_json::Map::new();
    for key in ["tag", "role", "name", "type", "color"] {
        if let Some(value) = observed[key].as_str().filter(|s| !s.is_empty()) {
            target.insert(key.into(), json!(crate::completion::excerpt(value, 160)));
        }
    }
    let bounds: serde_json::Map<_, _> = ["x", "y", "width", "height"]
        .into_iter()
        .filter(|key| observed["bounds"][key].is_number())
        .map(|key| (key.into(), observed["bounds"][key].clone()))
        .collect();
    if !bounds.is_empty() {
        target.insert("bounds".into(), Value::Object(bounds));
    }
    Value::Object(target)
}

fn repetition_feedback(session: &SessionData, run: &Run) -> Result<Option<Message>> {
    let Some((index, invocation)) = run
        .invocations
        .iter()
        .enumerate()
        .rev()
        .find(|(_, i)| is_browser(&i.call) && i.result.is_some())
    else {
        return Ok(None);
    };
    let count = crate::harness::repeated_calls(session, run, &invocation.call)?;
    if count < run.limits.max_repeated_calls.saturating_sub(1).max(1) {
        return Ok(None);
    }
    let result = invocation.result.as_ref().unwrap();
    let args = &invocation.call.arguments;
    let observed_target = result
        .data
        .get("action_target")
        .filter(|v| v.is_object())
        .or_else(|| {
            let reference = args["target"]["ref"].as_str()?;
            run.invocations[..index]
                .iter()
                .rev()
                .filter_map(|i| i.result.as_ref())
                .find(|r| {
                    r.data["browser_id"] == args["browser_id"]
                        && r.data["observation_id"].is_string()
                })?
                .data["controls"]
                .as_array()?
                .iter()
                .find(|c| c["ref"] == reference)
        });
    let target = target_summary(observed_target.unwrap_or(&Value::Null));
    Ok(Some(crate::harness::feedback(
        "browser_repetition",
        json!({
            "browser_id":args["browser_id"],"tool":invocation.call.name,"action":args["action"],
            "target":target,"count":count,"limit":run.limits.max_repeated_calls,
            "remaining_identical_calls":run.limits.max_repeated_calls.saturating_sub(count),
            "last_result":result.status,
            "detail":"This action/target repeats without new evidence. A new ref does not mean a new control. Reuse successful effects; perform the next task operation from the latest observation instead of selecting the same control again. At zero remaining calls, another identical action is blocked."
        }),
    )))
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

/// Lossless, request-only alternative for dense interfaces on small contexts.
/// Named columns retain the same targeting/visual evidence without repeating
/// the keys of every rectangle. Unknown attributes stay in the last column.
pub(crate) fn tabulate_controls(
    messages: &mut [Message],
    browser_calls: &BTreeSet<String>,
) -> bool {
    let mut changed = false;
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
        let Some(controls) = result["data"]["controls"].as_array() else {
            continue;
        };
        // Unrecognized layouts keep their original representation. Never lose
        // a new field inside bounds when the browser receipt evolves.
        if controls.is_empty()
            || !controls.iter().all(|c| {
                c.is_object()
                    && c["bounds"].as_object().is_some_and(|b| {
                        b.len() == 4
                            && ["x", "y", "width", "height"]
                                .iter()
                                .all(|key| b.get(*key).is_some_and(Value::is_number))
                    })
            })
        {
            continue;
        }
        let mut tags = BTreeMap::<&str, usize>::new();
        let tagged = controls.iter().all(|control| {
            if let Some(tag) = control["tag"].as_str() {
                *tags.entry(tag).or_default() += 1;
                true
            } else {
                false
            }
        });
        let default_tag =
            tagged.then(|| tags.into_iter().max_by_key(|(_, count)| *count).unwrap().0);
        let rows: Vec<_> = controls
            .iter()
            .map(|control| {
                let mut attributes = control.as_object().unwrap().clone();
                let mut row = Vec::new();
                if default_tag
                    .is_some_and(|tag| attributes.get("tag").and_then(Value::as_str) == Some(tag))
                {
                    attributes.remove("tag");
                }
                for key in ["ref", "name", "role"] {
                    // Null denotes an absent string column. Non-string extensions
                    // (including explicit null) remain verbatim in attributes.
                    row.push(if attributes.get(key).is_some_and(Value::is_string) {
                        attributes.remove(key).unwrap()
                    } else {
                        Value::Null
                    });
                }
                let bounds = attributes.remove("bounds").unwrap();
                row.push(json!([
                    bounds["x"],
                    bounds["y"],
                    bounds["width"],
                    bounds["height"]
                ]));
                row.push(Value::Object(attributes));
                Value::Array(row)
            })
            .collect();
        result["data"]["controls"] = json!({
            "columns":["ref","name","role","bounds","other_attributes"],
            "bounds_columns":["x","y","width","height"],
            "defaults":default_tag.map(|tag| json!({"tag":tag})).unwrap_or_else(|| json!({})),
            "rows":rows
        });
        let content = result.to_string();
        if content.len() < message.content.len() {
            message.content = content;
            changed = true;
        }
    }
    changed
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
    fn dense_control_table_preserves_all_evidence_and_only_projects_browser_receipts() {
        let controls: Vec<_> = (0..64)
            .map(|n| {
                json!({
                    "ref":format!("e{n}"),"tag":"button","name":format!("Öffnen {n} 🚀"),
                    "role":"menuitemcheckbox","checked":false,"selected":true,
                    "protected":true,"consequential":true,"disabled":false,
                    "bounds":{"x":12.125,"y":n * 9,"width":20.25,"height":8},
                    "custom":{"value":null,"array":[false,"mixed"]}
                })
            })
            .collect();
        let mut controls = controls;
        controls[0].as_object_mut().unwrap().remove("name");
        controls[1]["name"] = Value::Null;
        controls[2]["role"] = json!({"extension":true});
        controls[3]["tag"] = json!("canvas");
        let result = ToolResult::ok(json!({"controls":controls,"observation_id":"current",
            "error":"Action failed", "text":"Keep this evidence", "action_executed":false}));
        let original = Message::tool("browser", &result);
        let mut messages = vec![
            original.clone(),
            Message::tool("unrelated", &result),
            Message::text(Role::User, original.content.clone()),
        ];
        let source = messages.clone();
        assert!(tabulate_controls(&mut messages, &["browser".into()].into()));
        assert_eq!(messages[1..], source[1..]);
        let mut projected: Value = serde_json::from_str(&messages[0].content).unwrap();
        let table = &projected["data"]["controls"];
        assert_eq!(table["rows"].as_array().unwrap().len(), controls.len());
        let restored: Vec<_> = table["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let mut control = table["defaults"].clone();
                control
                    .as_object_mut()
                    .unwrap()
                    .extend(row[4].as_object().unwrap().clone());
                for (column, key) in ["ref", "name", "role"].iter().enumerate() {
                    if row[column].is_string() {
                        control[key] = row[column].clone();
                    }
                }
                control["bounds"] =
                    json!({"x":row[3][0],"y":row[3][1],"width":row[3][2],"height":row[3][3]});
                control
            })
            .collect();
        assert_eq!(restored, controls);
        projected["data"]["controls"] = json!(restored);
        assert_eq!(
            projected,
            serde_json::from_str::<Value>(&original.content).unwrap()
        );
        let once = messages.clone();
        assert!(!tabulate_controls(
            &mut messages,
            &["browser".into()].into()
        ));
        assert_eq!(messages, once);
        // Future or malformed bounds must never be partially projected.
        for bounds in [
            json!({"x":1,"y":2}),
            json!({"x":1,"y":2,"width":3,"height":4,"zoom":2}),
        ] {
            let mut unknown = result.clone();
            unknown.data["controls"][0]["bounds"] = bounds;
            let mut messages = vec![Message::tool("browser", &unknown)];
            let original = messages.clone();
            assert!(!tabulate_controls(
                &mut messages,
                &["browser".into()].into()
            ));
            assert_eq!(messages, original);
        }
    }

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
    fn alternating_browser_states_trigger_strategy_recovery_across_tabs_and_resume() {
        let mut run = run();
        let observe = |run: &mut Run, browser: &str, tab: u64, state: &str| {
            let call = add(run, browser, tab, state, Some(format!("frame-{state}")));
            let invocation = run.invocations.last_mut().unwrap();
            // Different pointer calls must not hide a recurring unchanged UI.
            invocation.call.name = "browser.action".into();
            invocation.call.arguments["action"] = json!("click_at");
            invocation.call.arguments["x"] = json!(invocation.message_index);
            invocation.call.arguments["y"] = json!(650);
            let data = &mut invocation.result.as_mut().unwrap().data;
            data["observation_id"] = json!(call.id);
            data["action_target"] = json!({"name":"Current method","tag":"button"});
            data["controls"] = json!([
                {"ref":"old-a","name":"Current method","tag":"button"},
                {"ref":"old-b","name":"Alternative operation","tag":"button"},
                {"name":"Disabled operation","disabled":true},
                {"name":"Protected input","protected":true},
                {"name":"Submit data","submits":true},
                {"name":"Delete data","consequential":true},
                {"name":"Download data","download":true},
                {"name":"Selected operation","selected":true},
                {"tag":"canvas","bounds":{"x":60,"y":20,"width":600,"height":380}}
            ]);
        };
        observe(&mut run, "b", 1, "A");
        observe(&mut run, "b", 1, "B");
        for n in 0..6 {
            observe(&mut run, "other", 1, &format!("new{n}"));
            observe(&mut run, "b", 2, &format!("tab{n}"));
            observe(&mut run, "b", 1, if n % 2 == 0 { "A" } else { "B" });
            assert_eq!(
                super::repetitions(&run, &run.invocations.last().unwrap().call),
                1
            );
            assert_eq!(super::strategy_feedback(&run).is_some(), n == 5);
        }
        let warning = super::strategy_feedback(&run).unwrap();
        let data: Value = serde_json::from_str(&warning.content).unwrap();
        assert_eq!(data["data"]["attempts_without_new_state"], 6);
        assert_eq!(
            data["data"]["untried_control_names"],
            json!(["Alternative operation"])
        );
        assert_eq!(data["data"]["recent_actions"].as_array().unwrap().len(), 4);
        assert_eq!(data["data"]["visual_surface_bounds"][0]["height"], 380);
        assert!(!warning.content.contains("old-a"));
        assert!(!warning.content.contains("old-b"));
        let registry = crate::tools::ToolRegistry::builtins().unwrap();
        let original: Vec<_> = ["browser.action", "browser.observe"]
            .iter()
            .map(|name| registry.get(name).unwrap().definition())
            .collect();
        let mut offered = original.clone();
        super::bound_strategy_tools(&run, &mut offered);
        assert_eq!(offered[1].input_schema, original[1].input_schema);
        let coordinate = json!({"browser_id":"b","action":"click_at","x":400,"y":650});
        assert!(crate::tools::validate_arguments(&original[0].input_schema, &coordinate).is_ok());
        assert!(crate::tools::validate_arguments(&offered[0].input_schema, &coordinate).is_err());
        assert!(
            crate::tools::validate_arguments(
                &offered[0].input_schema,
                &json!({"browser_id":"b","action":"click","target":{"ref":"old-b"},"visual":true})
            )
            .is_ok()
        );
        assert!(
            crate::tools::validate_arguments(
                &offered[0].input_schema,
                &json!({"browser_id":"b","action":"click","target":{"ref":"old-a"}})
            )
            .is_err()
        );
        let restored: Run = serde_json::from_value(serde_json::to_value(&run).unwrap()).unwrap();
        assert_eq!(super::strategy_feedback(&restored), Some(warning));
        let mut inspected = restored.clone();
        observe(&mut inspected, "b", 1, "A");
        let inspection = inspected.invocations.last_mut().unwrap();
        inspection.call.arguments =
            json!({"browser_id":"b","action":"click","target":{"ref":"old-b"}});
        inspection.request_input_schema = Some(offered[0].input_schema.clone());
        inspection.result.as_mut().unwrap().data["action_target"]["name"] =
            json!("Alternative operation");
        assert!(
            super::strategy_feedback(&inspected).is_none(),
            "reopening known alternative options must release gestures"
        );
        let prior = &restored.invocations.last().unwrap().call;
        assert_eq!(
            super::repetitions(&inspected, prior),
            super::repetitions(&restored, prior),
            "strategy transitions never reset the hard repetition guard"
        );
        let mut unavailable = restored.clone();
        unavailable
            .invocations
            .last_mut()
            .unwrap()
            .result
            .as_mut()
            .unwrap()
            .data["controls"] = json!([]);
        let mut no_alternatives = original.clone();
        super::bound_strategy_tools(&unavailable, &mut no_alternatives);
        assert_eq!(
            serde_json::to_value(no_alternatives).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
        // Replanning and pre-execution validation do not erase the episode.
        let mut extra = run.invocations.last().unwrap().clone();
        extra.call.name = "task.plan".into();
        extra.result = Some(ToolResult::ok(json!({"steps":[]})));
        run.invocations.push(extra.clone());
        assert!(super::strategy_feedback(&run).is_some());
        extra.call.name = "browser.action".into();
        extra.result = Some(ToolResult::error(
            Outcome::ValidationError,
            "invalid_browser_target",
        ));
        run.invocations.push(extra.clone());
        assert!(super::strategy_feedback(&run).is_some());
        // No claim of unchanged output after a possibly executed unknown call.
        extra.result = Some(ToolResult::error(Outcome::OutcomeUnknown, "transport_lost"));
        run.invocations.push(extra);
        assert!(super::strategy_feedback(&run).is_none());
        observe(&mut run, "b", 1, "changed work");
        assert!(super::strategy_feedback(&run).is_none());
        let mut restored_catalog = original.clone();
        super::bound_strategy_tools(&run, &mut restored_catalog);
        assert_eq!(
            serde_json::to_value(restored_catalog).unwrap(),
            serde_json::to_value(original).unwrap()
        );
    }
    #[test]
    fn repetition_warning_bounds_observed_data_and_echo_ownership() {
        let mut run = run();
        for n in 0..3 {
            let mut result = ToolResult::error(Outcome::OutcomeUnknown, "lost_connection");
            result.data = json!({"action_target":{
                "name":"é".repeat(5000),"text":"private text","value":"private field",
                "bounds":{"x":1,"y":2,"width":3,"height":4,"extra":"unbounded"}
            }});
            run.invocations.push(serde_json::from_value(json!({
                "invocation_id":format!("i{n}"),"step_id":"s","message_index":n,
                "call":{"id":format!("c{n}"),"name":"browser.action",
                    "arguments":{"browser_id":"b","action":"fill","text":"private value","target":{"label":"invented label"}}},
                "status":"finished","prepared":{},"result":result
            })).unwrap());
        }
        let session: SessionData = serde_json::from_value(json!({
            "schema_version":1,"id":"s","revision":0,"workspace":"/tmp","workspace_identity":"w",
            "sandbox_id":"b","principal":"p","profile":crate::profiles::AgentProfile::bundled("general-readonly").unwrap(),
            "profile_hash":"h","policy":crate::policy::HostPolicy::default(),"metadata":{},
            "messages":[],"runs":[run],"commands":{},"events":[],"skill_snapshots":{},"compactions":[]
        })).unwrap();
        let warning = super::feedback(&session, &run).unwrap().unwrap();
        let data: Value = serde_json::from_str(&warning.content).unwrap();
        assert_eq!(data["data"]["last_result"], "outcome_unknown");
        assert_eq!(data["data"]["remaining_identical_calls"], 0);
        assert!(warning.content.len() < 1200);
        for private in ["private", "invented label", "unbounded"] {
            assert!(!warning.content.contains(private));
        }
        let mut request = crate::providers::ModelRequest {
            reasoning_tier: None,
            messages: vec![warning.clone()],
            tools: vec![],
            max_output_tokens: 1024,
            output_schema: None,
        };
        assert!(crate::harness::is_feedback_echo(&session, &run, &request, &warning).unwrap());
        request.messages.clear();
        assert!(!crate::harness::is_feedback_echo(&session, &run, &request, &warning).unwrap());
        // Host policy may use a different hard limit; the warning cannot grant
        // another call or silently assume the default of three.
        run.limits.max_repeated_calls = 2;
        let data: Value =
            serde_json::from_str(&super::feedback(&session, &run).unwrap().unwrap().content)
                .unwrap();
        assert_eq!(data["data"]["limit"], 2);
        assert_eq!(data["data"]["remaining_identical_calls"], 0);
        run.limits.max_repeated_calls = 5;
        assert!(super::feedback(&session, &run).unwrap().is_none());
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
