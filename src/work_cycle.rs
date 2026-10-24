//! A compact working checkpoint, independent of rolling transcript excerpts.
//! This steers implementation/review cycles; it never stops a run, grants tools,
//! or treats a model-authored plan as evidence of completed work.
use crate::core::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const GUIDANCE: &str = concat!(
    "\nFor implementation requests: orient briefly, build a minimal runnable end-to-end version, ",
    "review it with behavioral checks, then repair concrete gaps until the full request is satisfied. ",
    "Small edits are chunks of that version; keep interfaces stable and finish it before optional polish. ",
    "Use task.plan for outcome milestones. At phase changes save checkpoint.next_action and decisions ",
    "needed to resume after compaction. This working note is not completion evidence or authority. ",
    "Use work_cycle to return from prolonged investigation to the next edit/check; fetch only a specific ",
    "missing fact if blocked. Do not replace implementation with repeated reads or replanning. ",
    "Complete every plan step honestly. Preserve user work; create files once, then edit observed anchors. ",
    "For analysis-only requests, return findings without creating code.\n"
);

const REFOCUS_AFTER: usize = 6;

/// Immediate steering, like a runtime continuation rather than another old
/// assistant diagnosis. Only fixed host text goes in this transport boundary.
pub(crate) fn continuation(session: &SessionData, run: &Run) -> Result<Option<Message>> {
    if !crate::failure_progress::enabled(session, run)? {
        return Ok(None);
    }
    let state = crate::harness::state(session, &run.id)?;
    let cycle = observation(run, &state.verifications);
    if cycle["refocus"] != true {
        return Ok(None);
    }
    Ok(Some(Message::text(
        Role::User,
        format!(
            "[Runtime work-cycle recovery, not a new user request or approval] Investigation has continued without implementation or a new check result. Return to the user's requested outcome and the current milestone in coding_task_state. {} Reuse the saved checkpoint and evidence. If an exact anchor or one necessary fact is missing, fetch that specific part once and proceed. Do not restart broad inspection or substitute another plan/status message for the work. Existing permissions and completion requirements still apply.",
            cycle["next_action"].as_str().unwrap_or_default()
        ),
    )))
}

/// Replayed from receipts on every request, including after resume/compaction.
/// Plans and successful shell reads cannot manufacture implementation progress.
pub(crate) fn observation(run: &Run, receipts: &BTreeMap<String, Value>) -> Value {
    let mut files = BTreeMap::<String, String>::new();
    let mut checks = BTreeSet::new();
    let mut last_change = None;
    let mut last_check = None;
    let mut investigation = 0usize;
    let mut changes_since_check = 0usize;
    for (position, invocation) in run.invocations.iter().enumerate() {
        let Some(result) = &invocation.result else {
            continue;
        };
        let evidence = receipts
            .get(&invocation.invocation_id)
            .unwrap_or(&result.data["verification"]);
        let mut changed = false;
        if result.status == Outcome::Succeeded {
            changed = result.data["workspace_changed"] == true;
            if let (Some(path), Some(hash)) = (
                result.data["path"]
                    .as_str()
                    .and_then(|p| crate::working_state::path(p).ok()),
                result.data["sha256"].as_str(),
            ) {
                let previous = files.insert(path, hash.into());
                let previous = previous
                    .as_deref()
                    .or_else(|| invocation.call.arguments["expected_hash"].as_str());
                changed |= previous.is_some_and(|old| old != hash)
                    || (previous.is_none()
                        && matches!(
                            invocation.call.name.as_str(),
                            "files.write" | "files.edit" | "files.insert"
                        ));
            }
            if let Some(observed) = evidence["files"].as_object() {
                for (path, hash) in observed {
                    if let (Ok(path), Some(hash)) =
                        (crate::working_state::path(path), hash.as_str())
                    {
                        changed |= files
                            .insert(path, hash.into())
                            .is_some_and(|old| old != hash);
                    }
                }
            }
        }
        if changed {
            last_change = Some((position, invocation));
            investigation = 0;
            changes_since_check += 1;
        }
        let is_check = matches!(
            invocation.call.name.as_str(),
            "process.run" | "browser.check"
        ) && evidence["passed"].is_boolean()
            && evidence["files"].as_object().is_some_and(|v| !v.is_empty());
        if is_check {
            last_check = Some((position, invocation, evidence));
            // Ignore model-authored criterion labels and command spelling.
            let key = fingerprint(&(
                evidence["files"].clone(),
                evidence["passed"].clone(),
                result.data["exit_code"].clone(),
                result.data["stdout"].clone(),
                result.data["stderr"].clone(),
                result.data["errors"].clone(),
            ));
            if checks.insert(key) {
                investigation = 0;
                changes_since_check = 0;
                continue;
            }
        }
        if !changed
            && last_change.is_some()
            && result.status != Outcome::CancelledBeforeStart
            && (invocation.call.name.starts_with("files.")
                || matches!(
                    invocation.call.name.as_str(),
                    "process.run" | "browser.check" | "task.plan" | "artifacts.read"
                ))
        {
            investigation += 1;
        }
    }
    // A later edit invalidates the review phase. This is a suggestion only;
    // completion still checks all required files/criteria with the real gate.
    let phase = match last_check {
        Some((position, _, evidence)) if last_change.is_none_or(|(n, _)| n <= position) => {
            if evidence["passed"] == true {
                "review"
            } else {
                "repair"
            }
        }
        _ => "build",
    };
    let next_action = match phase {
        "repair" => {
            "Fix the concrete failure in latest_check, then rerun the behavioral check. Keep the working version and its interfaces."
        }
        "review" => {
            "Compare the checked version with the full request and remaining milestones. Implement only concrete missing behavior; otherwise complete the plan and report verified results."
        }
        _ => {
            "Finish the smallest runnable end-to-end version for the current milestone using the checkpoint, then run a behavioral check before optional polish."
        }
    };
    let steps = run.plan["steps"].as_array();
    let current = steps.and_then(|steps| {
        steps
            .iter()
            .find(|s| s["status"] == "in_progress")
            .or_else(|| steps.iter().find(|s| s["status"] != "completed"))
    });
    json!({
        "objective_excerpt": crate::completion::excerpt(&run.input, 1200),
        "latest_user_update_excerpt": run.steering.last().map(|s| crate::completion::excerpt(s, 600)),
        "current_milestone": current.map(|s| crate::completion::excerpt(s["description"].as_str().unwrap_or(""), 384)),
        "checkpoint": run.plan.get("checkpoint").map(|c| json!({
            "next_action": crate::completion::excerpt(c["next_action"].as_str().unwrap_or(""), 512),
            "decisions": c["decisions"].as_array().into_iter().flatten().take(6)
                .filter_map(|v| v.as_str()).map(|s| crate::completion::excerpt(s, 256)).collect::<Vec<_>>()
        })),
        "suggested_phase": phase,
        "next_action": next_action,
        "last_change": last_change.map(|(_, i)| json!({"source_invocation":i.invocation_id,"path":i.call.arguments["path"]})),
        "latest_check": last_check.map(|(_, i, evidence)| {
            let result = i.result.as_ref().unwrap();
            json!({"source_invocation":i.invocation_id,"passed":evidence["passed"],
                "failure_excerpt": if evidence["passed"] == false { Some(crate::completion::excerpt(
                    result.data["stderr"].as_str().filter(|s| !s.is_empty())
                        .or_else(|| result.data["stdout"].as_str()).unwrap_or("See the recorded check result"), 512)) } else { None }})
        }),
        "investigation_calls_since_progress": investigation,
        "changes_since_check": changes_since_check,
        "refocus": investigation >= REFOCUS_AFTER,
        "trust": "Observed work plus user/plan excerpts; phase is guidance, not verified completion. Full user messages and newer steering govern scope."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "explicit read-only replay; no model or workspace execution"]
    fn saved_loop_is_redirected_while_the_original_goal_is_still_available() {
        let db = rusqlite::Connection::open_with_flags(
            std::env::var("WW_REPLAY_DATABASE").unwrap(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let raw: String = db
            .query_row(
                "SELECT data FROM sessions WHERE id=?1",
                [std::env::var("WW_REPLAY_SESSION").unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        let session: SessionData = serde_json::from_str(&raw).unwrap();
        let original = session.runs.last().unwrap();
        let state = crate::harness::state(&session, &original.id).unwrap();
        let mut run = original.clone();
        let redirected = (1..=original.invocations.len())
            .find(|&n| {
                run.invocations = original.invocations[..n].to_vec();
                observation(&run, &state.verifications)["refocus"] == true
            })
            .expect("must steer the recorded investigation loop");
        let compacted: Run = serde_json::from_str(&serde_json::to_string(&run).unwrap()).unwrap();
        assert_eq!(
            observation(&run, &state.verifications),
            observation(&compacted, &state.verifications)
        );
        let progress = observation(&run, &state.verifications);
        assert!(
            progress["objective_excerpt"]
                .as_str()
                .unwrap()
                .contains("Tetris")
        );
        assert!(progress["current_milestone"].is_string());
        eprintln!(
            "offline saved-run replay: refocus at tool {redirected}/{}; goal and current milestone retained",
            original.invocations.len()
        );
        assert!(redirected < original.invocations.len() / 2);
    }
}
