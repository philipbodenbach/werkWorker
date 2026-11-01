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

/// Each browser/tab has its own epoch. Seen semantic states cannot renew it
/// twice (e.g. blinking). Visual novelty has a finite allowance, even if an
/// animation produces infinitely many different frames.
pub(crate) fn repetitions(run: &Run, call: &ToolCall) -> usize {
    let browser = call.arguments["browser_id"].as_str().unwrap_or("");
    let current_scope = run
        .invocations
        .iter()
        .rev()
        .filter_map(|i| i.result.as_ref())
        .filter_map(|r| scope(&r.data))
        .find(|(id, _)| id == browser);
    let Some(current_scope) = current_scope else {
        return run
            .invocations
            .iter()
            .filter(|i| {
                crate::working_state::call_key(&i.call) == crate::working_state::call_key(call)
            })
            .count();
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
        .filter(|i| {
            crate::working_state::call_key(&i.call) == crate::working_state::call_key(call)
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

#[cfg(test)]
mod tests {
    use super::*;
    fn run() -> Run {
        serde_json::from_value(json!({"id":"r","status":"running","input":"Browse", "limits":Limits::default(),"started_at_ms":0,"deadline_ms":0,"history_start":0,"metrics":Metrics::default(),"attempts":[],"contexts":[],"invocations":[],"steering":[],"plan":null,"similar_grants":[],"repaired_output":false})).unwrap()
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
