//! Completion evidence is produced by tools, never by assistant prose.
use crate::{core::*, runtime::Workspace};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) fn coding(session: &SessionData) -> bool {
    session.profile.name == "coding-basic"
        || session.profile.skills.iter().any(|s| s == "coding-basic")
}
pub(crate) fn enabled(session: &SessionData, run: &Run) -> Result<bool> {
    Ok(coding(session) && crate::harness::config(session, run)?.coding_checks)
}

pub(crate) fn excerpt(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Rebuild from durable task state, never by trusting an assistant's summary.
pub(crate) fn observation(session: &SessionData, run: &Run) -> Result<Option<Message>> {
    if !enabled(session, run)? {
        return Ok(None);
    }
    let state = crate::harness::state(session, &run.id)?;
    let check =
        run.invocations.iter().rev().find(|i| {
            i.call.name == "process.run" && i.call.arguments.get("verification").is_some()
        });
    let observed = check.map(|check| {
        let evidence = state.verifications.get(&check.invocation_id);
        json!({"criteria":check.call.arguments["verification"]["criteria"],"result":check.result.as_ref().map(|r| json!({"status":r.status,"exit_code":r.data["exit_code"],"verification":evidence.unwrap_or(&r.data["verification"])}))})
    });
    // Keep file receipts at the current decision point. An old empty listing
    // or compacted assistant claim must not look newer than successful writes.
    let editing = session
        .profile
        .tools
        .iter()
        .any(|t| matches!(t.as_str(), "files.write" | "files.edit"));
    let files = if editing {
        file_observations(session, run)
    } else {
        Vec::new()
    };
    let mut repeated = BTreeMap::new();
    let mut considered = std::collections::BTreeSet::new();
    for invocation in run.invocations.iter().rev().take(128) {
        if !editing {
            break;
        }
        let key = fingerprint(&(&invocation.call.name, &invocation.call.arguments));
        if !considered.insert(key.clone()) {
            continue;
        }
        let count = crate::harness::repetitions(run, &invocation.call);
        if count >= 2
            && count >= run.limits.max_repeated_calls.saturating_sub(1)
            && repeated.len() < 4
        {
            repeated.entry(key)
                .or_insert_with(|| json!({"tool":invocation.call.name,"path":invocation.call.arguments["path"],"count":count,"limit":run.limits.max_repeated_calls}));
        }
    }
    let repairs = crate::working_state::repairs(session, run);
    if run.plan.is_null()
        && check.is_none()
        && files.is_empty()
        && repeated.is_empty()
        && repairs.is_empty()
    {
        return Ok(None);
    }
    Ok(Some(crate::harness::feedback(
        "coding_task_state",
        json!({
            "plan_excerpt": excerpt(&run.plan.to_string(), 4096),
            "verification_excerpt": excerpt(&serde_json::to_string(&observed)?, 3072),
            "files": files,
            "pending_repairs": repairs,
            "repeated_calls": repeated.into_values().collect::<Vec<_>>(),
            "detail":"Current run plan and verification. Latest observations per normalized path supersede older receipts, including not_found and missing directory entries. A missing file has no valid edit hash or anchor. Compaction does not change files. Repair a rejected edit with an exact observed anchor; never delete and rebuild to bypass it. Pending repairs keep shell workspace read-only until a corrected file change succeeds. Finish and verify remaining work."
        }),
    )))
}

fn file_observations(session: &SessionData, run: &Run) -> Vec<Value> {
    crate::working_state::bounded_files(session, run)
}

fn normalized_path(path: &str) -> Result<String> {
    crate::working_state::path(path)
}

pub(crate) async fn verification_files(
    args: &Value,
    workspace: &Workspace,
) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    if let Some(paths) = args["verification"]["paths"].as_array() {
        for path in paths {
            let path = normalized_path(path.as_str().ok_or_else(|| {
                WorkerError::new("invalid_verification", "Verification paths must be strings")
            })?)?;
            files.insert(path.clone(), hash(workspace.read(&path, 1_048_576).await?));
        }
    }
    Ok(files)
}

pub(crate) fn bound_tools(tools: &mut [ToolDefinition]) {
    for tool in tools {
        let fields: &[(&str, u64)] = match tool.name.as_str() {
            "files.write" => {
                tool.input_schema["properties"]
                    .as_object_mut()
                    .unwrap()
                    .remove("expected_hash");
                tool.description = "Create a NEW workspace file in a small working increment. Existing files must use files.edit with an observed unique anchor. Do not rewrite an existing implementation or embed it in a shell command.".into();
                &[("content", 4096)]
            }
            "files.edit" => &[("old", 2048), ("new", 4096)],
            _ => &[],
        };
        for (field, ceiling) in fields {
            let property = &mut tool.input_schema["properties"][field];
            property["maxLength"] = json!(
                property["maxLength"]
                    .as_u64()
                    .unwrap_or(*ceiling)
                    .min(*ceiling)
            );
        }
    }
}

/// A receipt is useful only for the current bytes. Check every source and test
/// named by that receipt, including unmodified dependencies supplied by the model.
pub(crate) async fn findings(
    run: &Run,
    receipts: &BTreeMap<String, Value>,
    workspace: &Workspace,
    require_receipt: bool,
) -> Result<Option<String>> {
    let open: Vec<_> = run.plan["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["status"] != "completed")
        .map(|s| s["description"].as_str().unwrap_or("unnamed step"))
        .collect();
    if !open.is_empty() {
        return Ok(Some(format!(
            "Required plan steps are still open: {}. Continue implementing and testing; update the plan only after completing the work.",
            excerpt(&open.join("; "), 2048)
        )));
    }
    // An explicit host acceptance command runs through the ordinary completion
    // gate after this plan check; do not require a duplicate model-selected test.
    if !require_receipt {
        return Ok(None);
    }
    let touched: BTreeMap<_, _> = run
        .invocations
        .iter()
        .filter(|i| matches!(i.call.name.as_str(), "files.write" | "files.edit"))
        .filter_map(|i| i.result.as_ref())
        .filter(|r| r.status == Outcome::Succeeded)
        .filter_map(|r| {
            Some((
                normalized_path(r.data["path"].as_str()?).ok()?,
                r.data["sha256"].as_str()?.to_owned(),
            ))
        })
        .collect();
    let work = !touched.is_empty()
        || run.invocations.iter().any(|i| {
            matches!(i.call.name.as_str(), "process.run" | "process.start")
                && i.result
                    .as_ref()
                    .is_some_and(|r| r.status == Outcome::Succeeded)
        });
    if !work {
        return Ok(None);
    }
    let Some((position, invocation, result)) = run
        .invocations
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, i)| {
            i.call.name == "process.run" && i.call.arguments.get("verification").is_some()
        })
        .find_map(|(n, i)| i.result.as_ref().map(|r| (n, i, r)))
    else {
        return Ok(Some("The implementation has no recorded behavioral verification. Use process.run with verification:{criteria:[actual acceptance criteria],paths:[all relevant source and test files, relative to workspace]}. Run assertions through real startup/input paths, inspect failures, fix them, and repeat. For an interactive app, syntax alone does not verify behavior. Do not claim completion if checks cannot run; report the blocker via interaction.ask.".into()));
    };
    let evidence = receipts
        .get(&invocation.invocation_id)
        .unwrap_or(&result.data["verification"]);
    if result.status != Outcome::Succeeded || evidence["passed"] != true {
        return Ok(Some("The latest verification failed or modified the files being verified. Inspect its result, repair the implementation/test, and rerun against unchanged source files.".into()));
    }
    if run.invocations[position + 1..].iter().any(|i| {
        matches!(i.call.name.as_str(), "process.run" | "process.start")
            && i.result
                .as_ref()
                .is_some_and(|r| r.status == Outcome::Succeeded)
    }) {
        return Ok(Some("A process ran after verification and may have changed the implementation. Verify the final file state again.".into()));
    }
    let files: BTreeMap<String, String> =
        serde_json::from_value(evidence["files"].clone()).unwrap_or_default();
    if files.is_empty() {
        return Ok(Some("Verification does not cover every changed file. Include the implementation and test paths and rerun the behavioral checks.".into()));
    }
    for path in touched.keys().filter(|path| !files.contains_key(*path)) {
        // Consolidation/renames can remove earlier outputs. Verify every
        // remaining changed file; never require executing a deleted source.
        // A read/permission/size error is not proof of intentional absence.
        if !workspace
            .read(path, 1_048_576)
            .await
            .is_err_and(|e| e.code == "not_found")
        {
            return Ok(Some("Verification does not cover every remaining changed file. Include the implementation and test paths and rerun the behavioral checks.".into()));
        }
    }
    for (path, expected) in files {
        if !workspace
            .read(&path, 1_048_576)
            .await
            .is_ok_and(|bytes| hash(bytes) == expected)
        {
            return Ok(Some(format!(
                "Verification is stale: {path} changed or disappeared. Run the relevant checks again on the current implementation."
            )));
        }
    }
    Ok(None)
}

pub(crate) fn progress(run: &Run) -> String {
    // Repeating the same write/check is not new progress. Invocation IDs and
    // growing stdout must not reset the stalled-final guard.
    let files: BTreeMap<_, _> = run
        .invocations
        .iter()
        .filter(|i| matches!(i.call.name.as_str(), "files.write" | "files.edit"))
        .filter_map(|i| i.result.as_ref())
        .filter(|r| r.status == Outcome::Succeeded)
        .filter_map(|r| Some((r.data["path"].as_str()?, r.data["sha256"].as_str()?)))
        .collect();
    let check = run.invocations.iter().rev()
        .find(|i| i.call.name == "process.run" && i.call.arguments.get("verification").is_some())
        .map(|i| json!({"arguments":i.call.arguments,"result":i.result.as_ref().map(|r| json!({"status":r.status,"verification":r.data["verification"],"exit_code":r.data["exit_code"]}))}));
    fingerprint(&(&run.plan, files, check))
}
