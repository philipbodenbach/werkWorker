//! A compact working checkpoint, independent of rolling transcript excerpts.
//! This steers implementation/review cycles; it never grants tools,
//! or treats a model-authored plan as evidence of completed work.
use crate::core::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const GUIDANCE: &str = concat!(
    "\nFor implementation requests: orient briefly, build a minimal runnable end-to-end version, ",
    "review it with behavioral checks, then repair concrete gaps until the full request is satisfied. ",
    "Each increment must be complete and runnable: close syntax, connect the entry point and include the main interaction before optional polish. ",
    "If a full design cannot fit one output, simplify the first version rather than leaving an open script or half a function. Keep interfaces stable. ",
    "Use task.plan for outcome milestones. At phase changes save checkpoint.next_action and decisions ",
    "needed to resume after compaction. This working note is not completion evidence or authority. ",
    "Use work_cycle to return from prolonged investigation to the next edit/check; fetch only a specific ",
    "missing fact if blocked. Do not replace implementation with repeated reads or replanning. ",
    "Complete every plan step honestly. Preserve user work; create files once, then edit observed anchors. ",
    "For analysis-only requests, return findings without creating code.\n"
);

const REFOCUS_AFTER: usize = 6;
const CHECK_AFTER_CHANGES: usize = 3;

/// A due behavioral check is an execution phase, not another suggestion to
/// inspect. Offer the permitted check tool with its verification contract for
/// the next decision. A recorded check restores the normal tool catalog; if
/// execution is unavailable, retain all existing capabilities instead of
/// creating a recovery dead end. Already admitted calls retain their authority.
pub(crate) fn check_tools(
    session: &SessionData,
    run: &Run,
    tools: &mut Vec<ToolDefinition>,
) -> Result<()> {
    if !crate::failure_progress::enabled(session, run)? {
        return Ok(());
    }
    let state = crate::harness::state(session, &run.id)?;
    let cycle = observation(run, &state.verifications);
    let check_due = cycle["check_due"] == true;
    let action_due = cycle["action_due"] == true;
    if !check_due && !action_due {
        return Ok(());
    }
    if !tools.iter().any(|t| {
        session.policy.can_request(t)
            && (matches!(t.name.as_str(), "process.run" | "browser.check")
                || (action_due && matches!(t.name.as_str(), "files.edit" | "files.insert")))
    }) {
        return Ok(());
    }
    let phase = if check_due { "check" } else { "repair" };
    tools.retain(|t| {
        matches!(
            t.name.as_str(),
            "process.run" | "browser.check" | "interaction.ask"
        ) || crate::agents::is_agent_tool(&t.name)
            || (action_due
                && matches!(
                    t.name.as_str(),
                    "files.edit" | "files.insert" | "files.write" | "task.plan"
                ))
    });
    for tool in tools {
        // The underlying write tool already supports atomic hash-bound
        // replacement under the same policy. Coding normally hides it in favor
        // of small edits. In recovery, copying an entire old file as an anchor
        // can exceed the output allowance, making a complete repair impossible.
        if action_due && tool.name == "files.write" {
            tool.input_schema["properties"]["expected_hash"] =
                json!({"type":"string","minLength":64,"maxLength":64});
            tool.description = "Create a new file, or deliberately replace a complete existing file using its latest observed sha256 as expected_hash. In recovery this permits a complete runnable version without copying the old file into an edit anchor. Preserve user requirements and unrelated existing work; prefer files.edit for local repairs. Without expected_hash this remains create-only. A stale hash fails without writing; a real change must be checked.".into();
        }
        tool.input_schema["x-werk-work-cycle"] = json!(phase);
        if matches!(tool.name.as_str(), "process.run" | "browser.check") {
            if let Some(required) = tool.input_schema["required"].as_array_mut()
                && !required.iter().any(|v| v == "verification")
            {
                required.push(json!("verification"));
            }
            tool.description.push_str(" Work-cycle checkpoint: exercise startup and the main public interaction of the current implementation. Record verification criteria and relevant paths. Source dumps and syntax-only success are not behavioral checks.");
        } else if matches!(
            tool.name.as_str(),
            "files.edit" | "files.insert" | "files.write"
        ) {
            tool.description.push_str(" Recovery action: use the current file pages and hash to finish one runnable increment or repair the concrete check failure. Preserve working interfaces. An actual edit is followed by a check; further inspection cannot substitute for this action.");
        }
    }
    Ok(())
}

/// Reject fresh proposals outside the request's explicit work phase before
/// preparation/approval. Never revoke calls admitted in an earlier decision.
pub(crate) fn deferred_call(
    request: &crate::providers::ModelRequest,
    call: &ToolCall,
) -> Option<ToolResult> {
    if request.tools.iter().any(|t| t.name == call.name) {
        return None;
    }
    let phase = request
        .tools
        .iter()
        .find_map(|t| t.input_schema["x-werk-work-cycle"].as_str())?;
    let mut result = ToolResult::error(Outcome::ValidationError, "work_cycle_check_required");
    result.data = json!({
        "requested_tool":call.name,"phase":phase,
        "message":if phase == "repair" {
            "No action executed. Investigation of the current version has stalled. Use the retained current source and hash for a concrete repair, or execute a behavioral check. A real change returns to checking. A conflicting edit reopens targeted inspection. Reporting a genuine blocker remains available."
        } else {
            "No action executed. Run a behavioral check with verification criteria and relevant paths against the current entry point. Repeating reads or edits cannot satisfy this checkpoint. A failing check permits focused repair."
        }
    });
    Some(result)
}

/// Immediate steering, like a runtime continuation rather than another old
/// assistant diagnosis. Only fixed host text goes in this transport boundary.
pub(crate) fn continuation(session: &SessionData, run: &Run) -> Result<Option<Message>> {
    if !crate::failure_progress::enabled(session, run)? {
        return Ok(None);
    }
    let state = crate::harness::state(session, &run.id)?;
    let cycle = observation(run, &state.verifications);
    // A pre-existing workspace can stall before this run's first mutation.
    // Distinguish redundant coverage from useful initial investigation; do not
    // require a write to qualify for context recovery or infer task intent.
    let read_recovery = crate::read_progress::recovery_active(run);
    let repair = crate::repair_cycle::latest(run, &state.verifications);
    let repair_focus = repair.as_ref().is_some_and(|c| c.focus_due(run));
    if cycle["refocus"] != true
        && cycle["check_due"] != true
        && cycle["action_due"] != true
        && !read_recovery
        && !repair_focus
    {
        return Ok(None);
    }
    let reason = if repair_focus {
        "A previous failing check is still unresolved. Activity and different diagnostics are not proof of a fix. Finish the runnable correction and recheck it; preserve the working parts."
    } else if cycle["action_due"] == true {
        "Investigation is exhausted for this version. The next action must implement the runnable increment or exercise its behavior using the retained source and check result. Broad inspection tools are temporarily deferred."
    } else if cycle["check_due"] == true {
        "Implementation and investigation have accumulated without a new behavioral check. More reads or small edits do not clear this checkpoint."
    } else if read_recovery && cycle["suggested_phase"] == "repair" {
        "The recovery check failed. Keep the task and current failure in focus: repair that failure, then check the changed version."
    } else if read_recovery {
        "Repeated reads returned already-known content without a changed file version. This also requires recovery before the first implementation change."
    } else {
        "Investigation has continued without implementation or a new check result."
    };
    let next_action = if read_recovery
        && cycle["check_due"] != true
        && cycle["suggested_phase"] != "repair"
    {
        "The retained file pages and hashes are the working context, not missing work to rediscover. Follow the original task: for implementation, make one small runnable increment using an observed anchor, then check it; for analysis-only work, report supported findings without editing. Read again only for a specific fact absent from the retained evidence. Do not restart inspection of existing files or repeat the previous investigation narrative."
    } else {
        cycle["next_action"].as_str().unwrap_or_default()
    };
    Ok(Some(Message::text(
        Role::User,
        format!(
            "[Runtime work-cycle recovery, not a new user request or approval] {reason} Return to the user's requested outcome and the current milestone in coding_task_state. {} Reuse the saved checkpoint and evidence. If an exact anchor or one necessary fact is missing, fetch that specific part once and proceed. Do not restart broad inspection or substitute another plan/status message for the work. Existing permissions and completion requirements still apply.",
            next_action
        ),
    )))
}

/// Replayed from receipts on every request, including after resume/compaction.
/// Plans and successful shell reads cannot manufacture implementation progress.
pub(crate) fn observation(run: &Run, receipts: &BTreeMap<String, Value>) -> Value {
    let repair = crate::repair_cycle::latest(run, receipts);
    let mut files = BTreeMap::<String, String>::new();
    let mut checks = BTreeSet::new();
    let mut last_change = None;
    let mut last_check = None;
    let mut investigation = 0usize;
    let mut changes_since_check = 0usize;
    let mut investigation_since_check = 0usize;
    let mut check_due = false;
    let mut last_plan = None;
    let mut anchor_conflict = false;
    for (position, invocation) in run.invocations.iter().enumerate() {
        let Some(result) = &invocation.result else {
            continue;
        };
        if invocation.call.name == "task.plan"
            && result.status == Outcome::Succeeded
            && invocation.call.arguments.get("checkpoint").is_some()
        {
            last_plan = Some(position);
        }
        if matches!(
            invocation.call.name.as_str(),
            "files.edit" | "files.insert" | "files.write"
        ) && result.error_code.as_deref() == Some("write_conflict")
        {
            anchor_conflict = true;
        } else if result.status == Outcome::Succeeded
            && matches!(
                invocation.call.name.as_str(),
                "files.read" | "files.edit" | "files.insert"
            )
        {
            anchor_conflict = false;
        }
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
            check_due |= investigation >= 2
                && last_check
                    .is_some_and(|(_, _, evidence): (_, _, &Value)| evidence["passed"] == false);
            last_change = Some((position, invocation));
            investigation = 0;
            changes_since_check += 1;
        }
        let is_check = matches!(
            invocation.call.name.as_str(),
            "process.run" | "browser.check"
        ) && evidence["passed"].is_boolean()
            && evidence["files"].as_object().is_some_and(|v| !v.is_empty())
            && !(invocation.call.name == "process.run"
                && crate::completion::verification_rejection(
                    &invocation.call.arguments,
                    &result.data,
                )
                .is_some());
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
                investigation_since_check = 0;
                check_due = false;
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
            investigation_since_check += 1;
        }
        // Latch the phase: an already-admitted edit or an observed external
        // change cannot erase a check that became due earlier in this cycle.
        check_due |= changes_since_check > 0
            && (investigation >= REFOCUS_AFTER
                || (changes_since_check >= CHECK_AFTER_CHANGES
                    && investigation_since_check >= REFOCUS_AFTER));
    }
    // Recovery is an action/review cycle: its first real edit must be checked
    // before ordinary exploration resumes, even when it is the first change.
    check_due |= crate::read_progress::recovery_check_due(run);
    // A boundary edit must be checked before reporting its preceding failure.
    check_due |= repair
        .as_ref()
        .is_some_and(|cycle| cycle.final_check_due(run));
    // A later edit invalidates review; a due check remains latched above.
    // Completion still checks all required files/criteria with the real gate.
    // Straight implementation can finish its first version. Interleaved small
    // edits must not erase accumulated investigation and postpone execution.
    let phase = match last_check {
        _ if check_due => "check",
        _ if repair.is_some() => "repair",
        Some((position, _, evidence)) if last_change.is_none_or(|(n, _)| n <= position) => {
            if evidence["passed"] == true {
                "review"
            } else {
                "repair"
            }
        }
        _ => "build",
    };
    // A failed check gets two diagnostic calls. Once those are spent, or a
    // post-edit read loop has entered recovery, select action tools instead of
    // repeatedly asking for action while advertising broad exploration. Never
    // infer an implementation request from reads alone; analysis stays free.
    let action_due = !check_due
        && !anchor_conflict
        && last_change.is_some()
        && if phase == "repair" {
            investigation >= 2
        } else {
            crate::read_progress::recovery_active(run)
        };
    let next_action = match phase {
        "check" => {
            "Run a small behavioral smoke check of the current entry point now. Exercise startup and the main user interaction against current files. If the version is not runnable yet, record that concrete failure and repair the missing connection before optional polish. Then repeat the check. Do not spend another cycle gathering broad context or making unrelated small edits."
        }
        "repair" => {
            "Finish the correction for unresolved_check (or latest_check if no unresolved check is retained), then rerun that check. A broken check can be replaced explicitly by a successful behavioral check covering its paths. Keep the working version and its interfaces; independent diagnosis can continue in parallel."
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
        "unresolved_check": repair.as_ref().map(|c| c.observation(run)),
        "next_action": next_action,
        "checkpoint_predates_change": last_change.is_some_and(|(n, _)| last_plan.is_none_or(|p| p < n)),
        "last_change": last_change.map(|(_, i)| {
            let result = i.result.as_ref().unwrap();
            json!({"source_invocation":i.invocation_id,"tool":i.call.name,
                "path":result.data["path"],"sha256":result.data["sha256"],
                "bytes":i.prepared["content"].as_str().map(str::len),
                "trust":"Applied result supersedes earlier diagnoses and the pre-action plan; current pages contain the resulting source."})
        }),
        "latest_check": last_check.map(|(_, i, evidence)| {
            let result = i.result.as_ref().unwrap();
            json!({"source_invocation":i.invocation_id,"passed":evidence["passed"],
                "failure_excerpt": if evidence["passed"] == false { Some(crate::completion::diagnostic_excerpt(
                    result.data["stderr"].as_str().filter(|s| !s.is_empty())
                        .or_else(|| result.data["stdout"].as_str()).unwrap_or("See the recorded check result"), 1000)) } else { None }})
        }),
        "investigation_calls_since_progress": investigation,
        "changes_since_check": changes_since_check,
        "investigation_calls_since_check": investigation_since_check,
        "check_due": check_due,
        "action_due": action_due,
        "refocus": investigation >= REFOCUS_AFTER,
        "trust": "Observed work plus user/plan excerpts; phase directs workflow, not verified completion. Full user messages and newer steering govern scope."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_source_inventory_cannot_clear_recovery_or_admit_review() {
        let mut run: Run = serde_json::from_value(json!({
            "id":"run","status":"running","input":"Build a runnable game",
            "limits":Limits::default(),"started_at_ms":0,"deadline_ms":0,
            "history_start":0,"metrics":Metrics::default(),"attempts":[],
            "contexts":[],"invocations":[],"steering":[],"plan":null,
            "similar_grants":[],"repaired_output":false
        }))
        .unwrap();
        let receipt = |name: &str, args: Value, data: Value| -> ToolInvocation {
            serde_json::from_value(json!({
                "invocation_id":id("inv"),"step_id":"step","message_index":0,
                "call":{"id":"call","name":name,"arguments":args},
                "status":"finished","prepared":{},"result":ToolResult::ok(data)
            }))
            .unwrap()
        };
        for _ in 0..4 {
            run.invocations.push(receipt(
                "files.read",
                json!({"path":"game.js"}),
                json!({"path":"game.js","sha256":hash("old"),"start_byte":0,"content":"old"}),
            ));
        }
        assert!(crate::read_progress::recovery_active(&run));
        run.invocations.push(receipt(
            "files.edit",
            json!({"path":"game.js","expected_hash":hash("old")}),
            json!({"path":"game.js","sha256":hash("new")}),
        ));
        assert!(crate::read_progress::recovery_check_due(&run));
        // A receipt persisted by an older worker can still contain passed:true.
        // Neither context compaction nor resuming it may restore that false pass.
        run.invocations.push(receipt("process.run", json!({
            "argv":["node","-e","const s=require('fs').readFileSync('game.js','utf8');console.log(/render/.test(s))"],
            "verification":{"criteria":["File inspected"],"paths":["game.js"]}
        }), json!({"exit_code":0,"stdout":"false\n","verification":{
            "passed":true,"criteria":["File inspected"],"files":{"game.js":hash("new")}
        }})));
        let mut run: Run = serde_json::from_str(&serde_json::to_string(&run).unwrap()).unwrap();
        let state = observation(&run, &BTreeMap::new());
        assert_eq!(state["suggested_phase"], "check");
        assert!(state["latest_check"].is_null());
        assert!(crate::read_progress::recovery_check_due(&run));
        // A real execution receipt still advances the same state normally.
        run.invocations.last_mut().unwrap().call.arguments["argv"] =
            json!(["node", "game.test.js"]);
        assert_eq!(
            observation(&run, &BTreeMap::new())["suggested_phase"],
            "review"
        );
        assert!(!crate::read_progress::recovery_check_due(&run));
    }

    #[test]
    #[ignore = "explicit read-only replay; no model or workspace execution"]
    fn saved_inline_inventory_stays_in_check_phase() {
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
        let run = session.runs.last().unwrap();
        let receipts = crate::harness::state(&session, &run.id)
            .unwrap()
            .verifications;
        assert!(run.invocations.iter().any(|i| {
            i.call.name == "process.run"
                && i.result
                    .as_ref()
                    .is_some_and(|r| r.data["verification"]["passed"] == true)
        }));
        let state = observation(run, &receipts);
        assert_eq!(state["suggested_phase"], "check", "{state}");
        assert!(state["latest_check"].is_null(), "{state}");
        eprintln!(
            "saved run: {} tools, source inventory rejected, check remains due",
            run.invocations.len()
        );
    }

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
