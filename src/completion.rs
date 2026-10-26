//! Completion evidence is produced by tools, never by assistant prose.
use crate::{core::*, runtime::Workspace};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Recognize obvious source inspection, not arbitrary program semantics. Unknown
/// commands still use the normal verification contract. In particular, loading
/// application code, VM evaluation and assertions are not classified as dumps.
fn inspection_only(arguments: &Value) -> bool {
    let Some(argv) = arguments["argv"].as_array() else {
        return false;
    };
    let Some(program) = argv.first().and_then(Value::as_str) else {
        return false;
    };
    let program = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let program = program.strip_suffix(".exe").unwrap_or(program);
    let reader = |name: &str| {
        matches!(
            name,
            "cat" | "head" | "tail" | "sed" | "rg" | "grep" | "wc" | "ls" | "stat" | "pwd"
        )
    };
    if reader(program) {
        return true;
    }
    if matches!(program, "node" | "nodejs")
        && matches!(
            argv.get(1).and_then(Value::as_str),
            Some("-e" | "--eval" | "-p" | "--print")
        )
        && let Some(script) = argv.get(2).and_then(Value::as_str)
    {
        return inline_source_inventory(script);
    }
    if matches!(program, "sh" | "bash" | "dash" | "zsh")
        && argv.get(1).and_then(Value::as_str) == Some("-c")
        && let Some(script) = argv.get(2).and_then(Value::as_str)
    {
        // Only a simple reader or pipeline. Do not guess about substitutions,
        // conditionals, assertions, redirections or compound shell programs.
        return !script.contains([';', '&', '$', '`', '>', '<', '\n', '\\', '\'', '"'])
            && script
                .split('|')
                .all(|part| part.split_whitespace().next().is_some_and(reader));
    }
    false
}

/// Recognize a narrow inline equivalent of cat/grep: fs reads, string queries
/// and console output only. This is not a JavaScript validator or a proof of
/// test semantics. Unknown calls/imports, indirect invocation and interpolation
/// retain the ordinary execution contract, including real VM/application tests.
fn inline_source_inventory(script: &str) -> bool {
    use regex::Regex;
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<(Regex, Regex, Regex)> = OnceLock::new();
    let (fs_import, calls, indirect) = PATTERNS.get_or_init(|| {
        (
            Regex::new(r#"\brequire\s*\(\s*(?:'(?:node:)?fs'|"(?:node:)?fs")\s*\)"#).unwrap(),
            Regex::new(r"[A-Za-z_$][A-Za-z0-9_$]*(?:\s*\.\s*[A-Za-z_$][A-Za-z0-9_$]*)*\s*\(")
                .unwrap(),
            Regex::new(r"[\]\)]\s*(?:\?\.)?\s*\(").unwrap(),
        )
    });
    if !fs_import.is_match(script) || script.contains('`') || indirect.is_match(script) {
        return false;
    }
    // Removing only the known builtin import leaves every application import
    // as an unknown call. Do not infer execution from criterion labels/stdout.
    let script = fs_import.replace_all(script, "__werk_fs");
    let mut read = false;
    for call in calls.find_iter(&script) {
        let name: String = call
            .as_str()
            .trim_end_matches('(')
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let member = name.contains('.') || script[..call.start()].trim_end().ends_with('.');
        let leaf = name.rsplit('.').next().unwrap();
        if member && leaf == "readFileSync" {
            read = true;
        } else if name != "console.log"
            && name != "console.error"
            && !(member
                && matches!(
                    leaf,
                    "test"
                        | "match"
                        | "includes"
                        | "indexOf"
                        | "slice"
                        | "substring"
                        | "split"
                        | "trim"
                        | "toString"
                ))
        {
            return false;
        }
    }
    read
}

pub(crate) fn verification_rejection(arguments: &Value, result: &Value) -> Option<&'static str> {
    if inspection_only(arguments) {
        return Some(
            "This command only inspected source or file metadata. Its output is available, but it is not a behavioral check and does not clear a pending check phase. Exercise the implementation entry point and assert its behavior.",
        );
    }
    let argv = arguments["argv"].as_array()?;
    let program = argv.first()?.as_str()?.rsplit('/').next()?;
    let option = argv.get(1).and_then(Value::as_str);
    let syntax_only = match program {
        "node" => matches!(option, Some("--check" | "-c")),
        "sh" | "bash" | "dash" | "zsh" => option == Some("-n"),
        "python" | "python3" => {
            option == Some("-m")
                && matches!(
                    argv.get(2).and_then(Value::as_str),
                    Some("py_compile" | "compileall")
                )
        }
        _ => false,
    };
    // A parser failure is concrete repair evidence. A successful parse does
    // not execute the entry point and must not unlock a behavioral checkpoint.
    (syntax_only && result["exit_code"] == 0).then_some("Syntax validation passed, but the implementation has not been exercised. This is not a behavioral check and does not clear the pending check phase. Execute the current entry point and assert the main interaction; record its failure if it is not runnable yet.")
}

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
    let check = run.invocations.iter().rev().find(|i| {
        matches!(i.call.name.as_str(), "process.run" | "browser.check")
            && i.call.arguments.get("verification").is_some()
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
        .any(|t| matches!(t.as_str(), "files.write" | "files.edit" | "files.insert"));
    let files = if editing {
        file_observations(session, run)
    } else {
        Vec::new()
    };
    let mut repeated = BTreeMap::new();
    let mut considered = std::collections::BTreeSet::new();
    let mut counts = None;
    for invocation in run.invocations.iter().rev().take(128) {
        if !editing {
            break;
        }
        let key = fingerprint(&(&invocation.call.name, &invocation.call.arguments));
        if !considered.insert(key.clone()) {
            continue;
        }
        let count = if matches!(
            invocation.call.name.as_str(),
            "process.status" | "agents.status"
        ) {
            // Active owned jobs have a special waiting exemption. Preserve the
            // exact argument/result check rather than generalizing that policy.
            crate::harness::repetitions(run, &invocation.call)
        } else {
            *counts
                .get_or_insert_with(|| crate::harness::repetition_counts(run))
                .get(&crate::working_state::call_key(&invocation.call))
                .unwrap_or(&0)
        };
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
    // Keep the checkpoint independently bounded, so a long step list cannot
    // crowd out the next action. Do not duplicate it in the plan excerpt.
    let mut plan = run.plan.clone();
    if let Some(plan) = plan.as_object_mut() {
        plan.remove("checkpoint");
    }
    Ok(Some(crate::harness::feedback(
        "coding_task_state",
        json!({
            "plan_excerpt": excerpt(&plan.to_string(), 4096),
            "work_cycle": crate::work_cycle::observation(run, &state.verifications),
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

pub(crate) fn bound_tools(tools: &mut [ToolDefinition], bound_sizes: bool) {
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
            "files.insert" => &[("content", 4096)],
            _ => &[],
        };
        for (field, ceiling) in fields {
            if !bound_sizes {
                continue;
            }
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
        .filter(|i| {
            matches!(
                i.call.name.as_str(),
                "files.write" | "files.edit" | "files.insert"
            )
        })
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
            matches!(i.call.name.as_str(), "process.run" | "browser.check")
                && i.call.arguments.get("verification").is_some()
        })
        .find_map(|(n, i)| i.result.as_ref().map(|r| (n, i, r)))
    else {
        return Ok(Some("The implementation has no recorded behavioral verification. Use process.run or browser.check with verification:{criteria:[actual acceptance criteria],paths:[all relevant source and test files, relative to workspace]}. Run assertions through real startup/input paths, inspect failures, fix them, and repeat. For an interactive app, syntax alone does not verify behavior. Do not claim completion if checks cannot run; report the blocker via interaction.ask.".into()));
    };
    let evidence = receipts
        .get(&invocation.invocation_id)
        .unwrap_or(&result.data["verification"]);
    if invocation.call.name == "process.run"
        && let Some(reason) = verification_rejection(&invocation.call.arguments, &result.data)
    {
        return Ok(Some(reason.into()));
    }
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
        .filter(|i| {
            matches!(
                i.call.name.as_str(),
                "files.write" | "files.edit" | "files.insert"
            )
        })
        .filter_map(|i| i.result.as_ref())
        .filter(|r| r.status == Outcome::Succeeded)
        .filter_map(|r| Some((r.data["path"].as_str()?, r.data["sha256"].as_str()?)))
        .collect();
    let check = run.invocations.iter().rev()
        .find(|i| matches!(i.call.name.as_str(), "process.run" | "browser.check") && i.call.arguments.get("verification").is_some()
            && !(i.call.name == "process.run" && i.result.as_ref().is_some_and(|r| verification_rejection(&i.call.arguments, &r.data).is_some())))
        .map(|i| json!({"arguments":i.call.arguments,"result":i.result.as_ref().map(|r| json!({"status":r.status,"verification":r.data["verification"],"exit_code":r.data["exit_code"]}))}));
    fingerprint(&(&run.plan, files, check))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_source_inventory_does_not_certify_a_runnable_candidate() {
        // Reduced from the 2026-10-26 saved run: exit 0 printed that all
        // important subsystems were missing, yet admitted an automatic review.
        let script = "const s=require('fs').readFileSync('tetris.html','utf8');console.log('has render:',/function render/.test(s));console.log('has audio:',/AudioContext/.test(s));console.log('has loop:',/requestAnimationFrame/.test(s));const m=s.match(/spawnBurst\\(p\\){/);console.log('tail anchor:',!!m);console.log(s.slice(-400))";
        for program in [
            "node",
            "/usr/bin/node",
            "C:\\Program Files\\nodejs\\node.exe",
        ] {
            let args = json!({"argv":[program,"-e",script],"verification":{"criteria":["Exit code 0 and output printed"],"paths":["tetris.html"]}});
            assert!(verification_rejection(&args, &json!({"exit_code":0,"stdout":"has render: false\nhas audio: false\nhas loop: false"})).is_some(), "{program}");
        }
    }

    #[test]
    fn direct_inspection_is_not_execution_and_unknown_programs_are_not_guessed() {
        for argv in [
            json!(["/bin/cat", "counter.sh"]),
            json!(["sh", "-c", "cat counter.sh | head -n 5"]),
        ] {
            assert!(inspection_only(&json!({"argv":argv})), "{argv}");
        }
        for argv in [
            json!([
                "node",
                "-e",
                "const fs=require('fs'),vm=require('vm');let g={};vm.runInNewContext(fs.readFileSync('game.js','utf8'),g);require('assert').equal(g.increment(0),1)"
            ]),
            json!(["node", "-e", "require('./game.js');console.log('started')"]),
            json!([
                "node",
                "-e",
                "const fs=require('fs');const s=fs.readFileSync('game.js','utf8');new Function(s)()"
            ]),
            json!(["sh", "-c", ". ./counter.sh; test \"$(increment 0)\" = 1"]),
            json!(["sh", "-c", "test \"$(cat state.txt)\" = 1"]),
            json!(["cargo", "test"]),
            json!(["./custom-smoke-test"]),
            json!([
                "node",
                "-e",
                "const fs=require('fs');const s=fs.readFileSync('game.js','utf8');require('./app').test(s)"
            ]),
            json!([
                "node",
                "-e",
                "const fs=require('fs');eval(fs.readFileSync('game.js','utf8'))"
            ]),
            json!([
                "node",
                "-e",
                "const fs=require('fs');(0,eval)(fs.readFileSync('game.js','utf8'))"
            ]),
            json!([
                "node",
                "-e",
                "const fs=require('fs');console.log(`${require('./app').start()} ${fs.readFileSync('game.js','utf8')}`)"
            ]),
        ] {
            assert!(!inspection_only(&json!({"argv":argv})), "{argv}");
        }
    }

    #[test]
    fn successful_syntax_is_not_behavior_but_parser_failures_allow_repair() {
        for argv in [
            json!(["node", "--check", "game.js"]),
            json!(["/bin/sh", "-n", "counter.sh"]),
            json!(["python3", "-m", "py_compile", "main.py"]),
        ] {
            let args = json!({"argv":argv});
            assert!(verification_rejection(&args, &json!({"exit_code":0})).is_some());
            assert!(verification_rejection(&args, &json!({"exit_code":1})).is_none());
        }
        assert!(
            verification_rejection(&json!({"argv":["node","game.js"]}), &json!({"exit_code":0}))
                .is_none()
        );
    }
}
