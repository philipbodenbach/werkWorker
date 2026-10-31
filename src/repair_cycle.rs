//! A failed check remains an obligation across edits, changed diagnostics and
//! compaction. File activity alone is not evidence that the failure was fixed.
use crate::core::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct RepairCycle {
    pub source_invocation: String,
    command: String,
    argv: Value,
    paths: BTreeSet<String>,
    rounds: BTreeSet<usize>,
    last_failure: String,
    browser_failure: Option<Value>,
    // Only explicit, path-covering descendants of the active obligation.
    // Keep each parent's paths so a replacement cannot drop intermediate scope.
    replacements: BTreeMap<String, BTreeSet<String>>,
}

fn command(invocation: &ToolInvocation) -> String {
    fingerprint(&(
        &invocation.call.name,
        &invocation.call.arguments["argv"],
        crate::working_state::path(invocation.call.arguments["cwd"].as_str().unwrap_or(".")),
        &invocation.call.arguments["path"],
        &invocation.call.arguments["actions"],
        &invocation.call.arguments["stdin"],
    ))
}

pub(crate) fn latest(run: &Run, receipts: &BTreeMap<String, Value>) -> Option<RepairCycle> {
    let mut cycle: Option<RepairCycle> = None;
    for invocation in &run.invocations {
        let Some(result) = &invocation.result else {
            continue;
        };
        if let Some(cycle) = &mut cycle {
            cycle.rounds.insert(invocation.message_index);
        }
        if !matches!(
            invocation.call.name.as_str(),
            "process.run" | "browser.check"
        ) {
            continue;
        }
        let evidence = receipts
            .get(&invocation.invocation_id)
            .unwrap_or(&result.data["verification"]);
        let Some(files) = evidence["files"].as_object().filter(|v| !v.is_empty()) else {
            continue;
        };
        // Invalid invocation/inspection is not an application failure. Apply
        // the same rule to historical receipts and newly executed checks.
        if invocation.call.name == "process.run"
            && crate::completion::verification_rejection(&invocation.call.arguments, &result.data)
                .is_some()
        {
            continue;
        }
        let paths: BTreeSet<_> = files
            .keys()
            .filter_map(|p| crate::working_state::path(p).ok())
            .collect();
        let check_command = command(invocation);
        let passed = result.status == Outcome::Succeeded && evidence["passed"] == true;
        let related = cycle.as_ref().is_some_and(|old| {
            old.paths.is_subset(&paths)
                && (old.command == check_command
                    || invocation.call.arguments["verification"]["replaces"]
                        .as_str()
                        .and_then(|id| old.replacements.get(id))
                        .is_some_and(|parent_paths| parent_paths.is_subset(&paths)))
        });
        if passed && related {
            cycle = None;
            continue;
        }
        // A command that changed the source is an implementation step, not a
        // failed test. Rejected/denied actions retain their existing guards.
        if result.status != Outcome::Succeeded
            || evidence["passed"] != false
            || result.data["workspace_changed"] == true
            || !(result.data["exit_code"]
                .as_i64()
                .is_some_and(|code| code != 0)
                || invocation.call.name == "browser.check")
        {
            continue;
        }
        let failure = result.data["stderr"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| result.data["stdout"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| result.data["errors"].to_string());
        let current = cycle.get_or_insert_with(|| RepairCycle {
            source_invocation: invocation.invocation_id.clone(),
            command: check_command,
            argv: if invocation.call.name == "browser.check" {
                json!({"tool":"browser.check","path":invocation.call.arguments["path"],"actions":invocation.call.arguments["actions"]})
            } else {
                invocation.call.arguments["argv"].clone()
            },
            paths: paths.clone(),
            rounds: BTreeSet::new(),
            last_failure: String::new(),
            browser_failure: None,
            replacements: [(invocation.invocation_id.clone(), paths.clone())].into(),
        });
        if related {
            current
                .replacements
                .insert(invocation.invocation_id.clone(), paths);
        }
        current.rounds.insert(invocation.message_index);
        current.last_failure = failure;
        current.browser_failure = result.data.get("failed_action").cloned();
    }
    cycle
}

impl RepairCycle {
    pub fn focus_due(&self, run: &Run) -> bool {
        self.rounds.len() >= run.limits.max_repeated_calls.max(1)
    }

    /// The edit at the repair limit gets one final check. The allowance cannot
    /// rearm itself: any further decision consumes it, including another edit.
    pub fn final_check_due(&self, run: &Run) -> bool {
        self.rounds.len() == stop_limit(run)
            && run.invocations.last().is_some_and(|i| {
                i.result.as_ref().is_some_and(|result| {
                    result.status == Outcome::Succeeded
                        && (result.data["workspace_changed"] == true
                            || matches!(
                                i.call.name.as_str(),
                                "files.edit" | "files.insert" | "files.write"
                            ) && result.data["sha256"].is_string()
                                && result.data["sha256"] != i.call.arguments["expected_hash"])
                })
            })
    }

    pub fn observation(&self, run: &Run) -> Value {
        json!({"source_invocation":self.source_invocation,
            "failed_command_excerpt":crate::completion::excerpt(&self.argv.to_string(), 1400),
            "paths":self.paths.iter().take(8).map(|p|crate::completion::excerpt(p,512)).collect::<Vec<_>>(),
            "decisions_without_resolution":self.rounds.len(),
            "decision_limit":stop_limit(run),
            "final_check_due":self.final_check_due(run),
            "last_failure_excerpt":crate::completion::diagnostic_excerpt(&self.last_failure, 1000),
            "browser_failure":self.browser_failure,
            "next_action":"Diagnose whether the application or the check failed before editing application code. Finish one correction, then rerun the check; fix a broken check using verification.replaces=source_invocation and the same paths. Prefer verification.kind=behavior with real assertions; syntax and inspection are not behavioral evidence. A successful replacement must cover unchanged files. Edits and unrelated passing commands do not resolve the failure. Delegate independent diagnosis while continuing separate work when useful."})
    }
}

fn stop_limit(run: &Run) -> usize {
    // A low exact-call limit must still leave enough decisions for diagnosis,
    // a complete multi-edit correction and its recheck.
    run.limits.max_repeated_calls.max(3).saturating_mul(4)
}

pub(crate) fn check(session: &SessionData, run: &Run) -> Result<()> {
    if crate::failure_progress::enabled(session, run)? {
        let state = crate::harness::state(session, &run.id)?;
        if let Some(cycle) = latest(run, &state.verifications)
            .filter(|c| c.rounds.len() >= stop_limit(run) && !c.final_check_due(run))
        {
            return Err(WorkerError::new(
                "repair_cycle_exhausted",
                format!(
                    "A failed check remains unresolved after {} model decisions, including edits and changed diagnostics. Work and receipts are retained; the task is incomplete. Failed check: {}. Latest failure: {}",
                    cycle.rounds.len(),
                    crate::completion::excerpt(&cycle.argv.to_string(), 400),
                    crate::completion::diagnostic_excerpt(&cycle.last_failure, 1000)
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> Run {
        serde_json::from_value(json!({
            "id":"run","status":"running","input":"Implement the requested behavior",
            "limits":Limits::continuous(),"started_at_ms":0,"deadline_ms":0,
            "history_start":0,"metrics":Metrics::default(),"attempts":[],"contexts":[],
            "invocations":[],"steering":[],"plan":null,"similar_grants":[],"repaired_output":false
        }))
        .unwrap()
    }

    fn receipt(index: usize, name: &str, args: Value, data: Value) -> ToolInvocation {
        serde_json::from_value(json!({
            "invocation_id":format!("inv_{index}"),"step_id":format!("step_{index}"),"message_index":index,
            "call":{"id":format!("call_{index}"),"name":name,"arguments":args},
            "status":"finished","prepared":{},"result":ToolResult::ok(data)
        })).unwrap()
    }

    fn check_receipt(index: usize, program: &str, passed: bool, revision: &str) -> ToolInvocation {
        receipt(
            index,
            "process.run",
            json!({"argv":[program],"verification":{"kind":"behavior","criteria":["Required behavior"],"paths":["app.txt"]}}),
            json!({"exit_code":if passed {0} else {1},"stderr":format!("diagnostic {index}"),"workspace_changed":false,
                "verification":{"passed":passed,"files":{"app.txt":hash(revision)}}}),
        )
    }

    #[test]
    fn edits_changed_errors_and_unrelated_success_cannot_erase_a_failed_check() {
        let mut run = run();
        run.invocations
            .push(check_receipt(0, "./acceptance", false, "old"));
        for n in 1..12 {
            let invocation = match n % 3 {
                0 => receipt(
                    n,
                    "files.edit",
                    json!({"path":"app.txt","expected_hash":hash(format!("v{n}"))}),
                    json!({"path":"app.txt","sha256":hash(format!("v{}",n+1))}),
                ),
                1 => check_receipt(
                    n,
                    &format!("./different-failure-{n}"),
                    false,
                    "different revision",
                ),
                _ => check_receipt(n, "./unrelated-success", true, "different revision"),
            };
            run.invocations.push(invocation);
        }
        let restored: Run = serde_json::from_slice(&serde_json::to_vec(&run).unwrap()).unwrap();
        let cycle = latest(&restored, &BTreeMap::new()).unwrap();
        assert_eq!(cycle.source_invocation, "inv_0");
        assert_eq!(cycle.rounds.len(), stop_limit(&restored));
        assert!(cycle.focus_due(&restored));
        assert_eq!(
            crate::work_cycle::observation(&restored, &BTreeMap::new())["suggested_phase"],
            "repair"
        );
        run.invocations
            .push(check_receipt(12, "./acceptance", true, "repaired"));
        assert!(latest(&run, &BTreeMap::new()).is_none());
    }

    #[test]
    fn browser_repair_preserves_failed_action_and_target_after_reload() {
        let mut run = run();
        let failure = json!({"index":3,"type":"expect_visible","selector":"#explanation",
            "last_interaction":{"index":1,"type":"click","selector":"#example"},
            "target":{"count":1,"visible":false,"hidden":true}});
        run.invocations.push(receipt(
            0,
            "browser.check",
            json!({"path":"tabs.html","actions":[
                {"type":"click","selector":"#example"},
                {"type":"expect_visible","selector":"#sample"},
                {"type":"expect_visible","selector":"#explanation"}],
                "verification":{"criteria":["Switch tabs"],"paths":["tabs.html"]}}),
            json!({"exit_code":1,"errors":["Timeout"],"failed_action":failure,
                "verification":{"passed":false,"files":{"tabs.html":"v1"}}}),
        ));
        let restored: Run = serde_json::from_slice(&serde_json::to_vec(&run).unwrap()).unwrap();
        let cycle = latest(&restored, &BTreeMap::new()).unwrap();
        let observation = cycle.observation(&restored);
        assert_eq!(observation["browser_failure"], failure);
        assert_eq!(observation["source_invocation"], "inv_0");
        let command: Value =
            serde_json::from_str(observation["failed_command_excerpt"].as_str().unwrap()).unwrap();
        assert_eq!(command["tool"], "browser.check");
        assert_eq!(command["path"], "tabs.html");
        // A correction must still succeed with the original path coverage.
        let mut corrected = run.invocations[0].clone();
        corrected.invocation_id = "inv_1".into();
        corrected.message_index = 1;
        corrected.call.arguments["actions"][2] = json!({"type":"click","selector":"#explain"});
        corrected.call.arguments["verification"]["replaces"] = json!("inv_0");
        run.invocations.push(corrected);
        assert!(latest(&run, &BTreeMap::new()).is_some());
        let result = run.invocations[1].result.as_mut().unwrap();
        result.data["verification"]["passed"] = json!(true);
        result.data["exit_code"] = json!(0);
        assert!(latest(&run, &BTreeMap::new()).is_none());
    }

    #[test]
    fn successful_replacement_resolves_a_chain_of_failed_replacements() {
        let mut run = run();
        run.invocations
            .push(check_receipt(0, "./original", false, "v1"));
        for index in 1..4 {
            let mut next = check_receipt(index, &format!("./correction-{index}"), index == 3, "v1");
            next.call.arguments["verification"]["replaces"] = json!(format!("inv_{}", index - 1));
            run.invocations.push(next);
            let restored: Run = serde_json::from_slice(&serde_json::to_vec(&run).unwrap()).unwrap();
            assert_eq!(latest(&restored, &BTreeMap::new()).is_none(), index == 3);
        }
    }

    #[test]
    fn replacement_chains_require_valid_ancestry_and_cover_every_parent_path() {
        for (kind, parent, paths) in [
            ("behavior", "unrelated", json!({"app.txt":"v1"})),
            ("inspection", "inv_0", json!({"app.txt":"v1"})),
            ("behavior", "inv_0", json!({"other.txt":"v1"})),
            (
                "behavior",
                "inv_0",
                json!({"app.txt":"v1","extra.txt":"v1"}),
            ),
        ] {
            let mut run = run();
            run.invocations
                .push(check_receipt(0, "./original", false, "v1"));
            let mut middle = check_receipt(1, "./changed", false, "v1");
            middle.call.arguments["verification"]["kind"] = json!(kind);
            middle.call.arguments["verification"]["replaces"] = json!(parent);
            middle.result.as_mut().unwrap().data["verification"]["files"] = paths.clone();
            run.invocations.push(middle);
            let mut last = check_receipt(2, "./passing", true, "v1");
            last.call.arguments["verification"]["replaces"] = json!("inv_1");
            run.invocations.push(last);
            assert!(
                latest(&run, &BTreeMap::new()).is_some(),
                "{kind} / {parent} / {paths}"
            );
        }
    }

    #[test]
    #[ignore = "read-only replay of a saved browser repair chain; no model requests or workspace execution"]
    fn saved_browser_replacement_chain_finishes_after_passing_correction() {
        let run: Run = serde_json::from_slice(
            &std::fs::read(std::env::var("WW_BROWSER_REPAIR_REPLAY").unwrap()).unwrap(),
        )
        .unwrap();
        let last = run.invocations.last().unwrap();
        assert_eq!(last.call.name, "browser.check");
        assert_eq!(
            last.result.as_ref().unwrap().data["verification"]["passed"],
            true
        );
        let cycle = latest(&run, &BTreeMap::new());
        assert!(
            cycle.is_none(),
            "passing correction left obligation: {:?}",
            cycle.map(|c| c.observation(&run))
        );
    }

    #[test]
    fn historical_untyped_process_success_cannot_replace_a_failed_browser_check() {
        let mut run = run();
        run.invocations.push(receipt(0, "browser.check",
            json!({"path":"index.html","actions":[{"type":"click","selector":"#next"}],"verification":{"criteria":["Next advances"],"paths":["index.html"]}}),
            json!({"passed":false,"exit_code":1,"errors":["index.html:4:1: SyntaxError: Unexpected token 'const'"],"verification":{"passed":false,"files":{"index.html":"v1"}}})));
        run.invocations.push(receipt(1, "process.run",
            json!({"argv":["python3","-c","s=open('/workspace/index.html').read();print(repr(s[:220]))"],"verification":{"replaces":"inv_0","criteria":["Inspect source"],"paths":["index.html"]}}),
            // Old saved receipts may incorrectly claim this inspection passed.
            json!({"exit_code":0,"verification":{"passed":true,"files":{"index.html":"v1"}}})));
        let restored: Run = serde_json::from_slice(&serde_json::to_vec(&run).unwrap()).unwrap();
        let cycle = latest(&restored, &BTreeMap::new()).unwrap();
        assert_eq!(cycle.source_invocation, "inv_0");
        assert!(cycle.last_failure.contains("index.html:4:1"));
        let mut corrected = run.invocations[0].clone();
        corrected.invocation_id = "corrected".into();
        corrected.message_index = 2;
        corrected.result = Some(ToolResult::ok(
            json!({"passed":true,"exit_code":0,"verification":{"passed":true,"files":{"index.html":"v2"}}}),
        ));
        run.invocations.push(corrected);
        assert!(latest(&run, &BTreeMap::new()).is_none());
    }

    #[test]
    #[ignore = "read-only replay of the source-inventory incident; no model or workspace execution"]
    fn saved_source_inventory_leaves_the_real_browser_failure_open() {
        let run: Run = serde_json::from_slice(
            &std::fs::read(std::env::var("WW_BROWSER_INSPECTION_REPLAY").unwrap()).unwrap(),
        )
        .unwrap();
        let last = run.invocations.last().unwrap();
        assert_eq!(last.call.name, "process.run");
        let data = &last.result.as_ref().unwrap().data;
        assert_eq!(data["verification"]["passed"], true);
        assert!(crate::completion::verification_rejection(&last.call.arguments, data).is_some());
        let cycle = latest(&run, &BTreeMap::new()).unwrap();
        assert_eq!(cycle.argv["tool"], "browser.check");
        assert!(cycle.last_failure.contains("Unexpected token 'const'"));
        assert!(cycle.rounds.len() >= stop_limit(&run));
        eprintln!(
            "Saved run: source inspection rejected; real browser failure still open after {} decisions",
            cycle.rounds.len()
        );
    }

    #[test]
    fn corrected_check_requires_explicit_replacement_success_and_path_coverage() {
        for (passed, kind, replaces, covered, resolved) in [
            (true, "behavior", "inv_0", true, true),
            (false, "behavior", "inv_0", true, false),
            (true, "syntax", "inv_0", true, false),
            (true, "inspection", "inv_0", true, false),
            (true, "behavior", "unknown", true, false),
            (true, "behavior", "inv_0", false, false),
        ] {
            let mut run = run();
            run.invocations
                .push(check_receipt(0, "./broken-test", false, "v1"));
            let mut replacement = check_receipt(1, "./correct-test", passed, "v1");
            replacement.call.arguments["verification"]["kind"] = json!(kind);
            replacement.call.arguments["verification"]["replaces"] = json!(replaces);
            if !covered {
                replacement.result.as_mut().unwrap().data["verification"]["files"] =
                    json!({"unrelated.txt":hash("v1")});
            }
            run.invocations.push(replacement);
            assert_eq!(
                latest(&run, &BTreeMap::new()).is_none(),
                resolved,
                "{kind} / {passed} / {replaces} / {covered}"
            );
        }
    }

    #[test]
    fn legacy_replacement_and_stdin_identity_follow_the_execution_contract() {
        let mut run = run();
        let mut first = check_receipt(0, "sh", false, "v1");
        first.call.arguments["stdin"] = json!("test important-behavior");
        run.invocations.push(first);
        let mut second = check_receipt(1, "sh", true, "v1");
        second.call.arguments["stdin"] = json!("true");
        run.invocations.push(second);
        assert!(latest(&run, &BTreeMap::new()).is_some());
        // kind is optional in the public schema. Do not silently ignore an
        // explicit replacement which the host accepted as verification.
        run.invocations[1].call.arguments["verification"]["replaces"] = json!("inv_0");
        assert!(latest(&run, &BTreeMap::new()).is_none());
        run.invocations[1].call.arguments["verification"]["replaces"] = Value::Null;
        run.invocations[1].call.arguments["stdin"] = json!("test important-behavior");
        assert!(latest(&run, &BTreeMap::new()).is_none());
    }

    #[test]
    fn invalid_invocations_and_inspection_failures_do_not_start_application_repairs() {
        let mut run = run();
        let mut invalid = check_receipt(0, "python3", false, "v1");
        invalid.call.arguments["argv"] = json!(["python3", "- <<'PY'\nprint(1)\nPY"]);
        run.invocations.push(invalid);
        let mut diagnostic = check_receipt(1, "./inspect", false, "v1");
        diagnostic.call.arguments["verification"]["kind"] = json!("inspection");
        run.invocations.push(diagnostic);
        assert!(latest(&run, &BTreeMap::new()).is_none());
        run.invocations
            .push(check_receipt(2, "./acceptance", false, "v1"));
        let cycle = latest(&run, &BTreeMap::new()).unwrap();
        assert_eq!(cycle.source_invocation, "inv_2");
        assert_eq!(cycle.rounds.len(), 1);
    }

    #[test]
    #[ignore = "explicit read-only replay of the malformed-command incident; no model or workspace execution"]
    fn saved_malformed_check_does_not_exhaust_later_application_repair() {
        let session: SessionData = serde_json::from_slice(
            &std::fs::read(std::env::var("WW_REPLAY_SNAPSHOT").unwrap()).unwrap(),
        )
        .unwrap();
        let run = session
            .runs
            .iter()
            .find(|r| {
                r.error
                    .as_ref()
                    .is_some_and(|e| e.code == "repair_cycle_exhausted")
            })
            .unwrap();
        let receipts = crate::harness::state(&session, &run.id)
            .unwrap()
            .verifications;
        let cycle = latest(run, &receipts).unwrap();
        assert!(cycle.rounds.len() < stop_limit(run));
        assert_eq!(
            cycle.source_invocation,
            run.invocations.last().unwrap().invocation_id
        );
        assert!(
            cycle.observation(run)["last_failure_excerpt"]
                .as_str()
                .unwrap()
                .contains("TypeError:")
        );
        eprintln!(
            "saved failure replay: obsolete invocation excluded; current check has {} decision(s), below {}",
            cycle.rounds.len(),
            stop_limit(run)
        );
    }

    #[test]
    fn parallel_tool_batches_count_as_one_decision_and_receipts_survive_truncation() {
        let mut run = run();
        let mut first = check_receipt(0, "./acceptance", false, "v1");
        let evidence = first
            .result
            .as_mut()
            .unwrap()
            .data
            .as_object_mut()
            .unwrap()
            .remove("verification")
            .unwrap();
        first.result.as_mut().unwrap().truncated = true;
        run.invocations.push(first);
        let receipts = BTreeMap::from([("inv_0".into(), evidence)]);
        for n in 1..20 {
            let mut invocation = receipt(
                n,
                "files.read",
                json!({"path":"app.txt"}),
                json!({"path":"app.txt","sha256":hash("v1")}),
            );
            invocation.message_index = 1;
            run.invocations.push(invocation);
        }
        assert_eq!(latest(&run, &receipts).unwrap().rounds.len(), 2);
    }

    #[test]
    #[ignore = "read-only replay of the supplied homepage incident; no model or workspace execution"]
    fn saved_homepage_boundary_edit_gets_one_final_check() {
        let session: SessionData = serde_json::from_slice(
            &std::fs::read(std::env::var("WW_REPLAY_SNAPSHOT").unwrap()).unwrap(),
        )
        .unwrap();
        let run = session.runs.last().unwrap();
        let state = crate::harness::state(&session, &run.id).unwrap();
        let cycle = latest(run, &state.verifications).unwrap();
        assert_eq!(cycle.rounds.len(), stop_limit(run));
        assert!(cycle.final_check_due(run));
        assert_eq!(
            crate::work_cycle::observation(run, &state.verifications)["check_due"],
            true
        );
        check(&session, run).unwrap();
        let mut after = run.clone();
        let mut another = receipt(999, "files.read", json!({"path":"index.html"}), json!({}));
        another.message_index = 999;
        after.invocations.push(another);
        assert_eq!(
            check(&session, &after).unwrap_err().code,
            "repair_cycle_exhausted"
        );
    }

    #[test]
    #[ignore = "explicit offline replay of a supplied session snapshot; no model or workspace execution"]
    fn saved_session_cannot_reset_repair_by_alternating_edits_and_diagnostics() {
        let session: SessionData = serde_json::from_slice(
            &std::fs::read(std::env::var("WW_REPLAY_SNAPSHOT").unwrap()).unwrap(),
        )
        .unwrap();
        let original = session.runs.last().unwrap();
        let receipts = crate::harness::state(&session, &original.id)
            .unwrap()
            .verifications;
        let mut run = original.clone();
        let mut stopped = None;
        for n in 1..=original.invocations.len() {
            run.invocations = original.invocations[..n].to_vec();
            if latest(&run, &receipts).is_some_and(|c| c.rounds.len() >= stop_limit(&run)) {
                stopped = Some(n);
                break;
            }
        }
        eprintln!(
            "offline repair-cycle replay: stop at tool {stopped:?} of {}",
            original.invocations.len()
        );
        assert!(stopped.is_some());
    }
}
