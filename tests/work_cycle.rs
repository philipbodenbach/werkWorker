use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{
    context::{ContextBuilder, DefaultContextBuilder, validate_messages},
    policy::*,
    profiles::AgentProfile,
    providers::*,
    *,
};

fn call(name: &str, arguments: Value) -> ScriptedTurn {
    ScriptedTurn::calls(
        "",
        vec![ToolCall {
            id: id("call"),
            name: name.into(),
            arguments,
        }],
    )
}

fn plan(status: &str) -> ScriptedTurn {
    call(
        "task.plan",
        json!({
            "steps":[{"id":"version","description":"Runnable counter with a verified increment","status":status}],
            "checkpoint":{"next_action":"Implement increment, then assert increment 0 produces 1",
                "decisions":["counter.sh defines increment; source it in the check"]}
        }),
    )
}

fn verify() -> ScriptedTurn {
    // The sandbox guarantees /bin/sh, not host-installed language runtimes
    // (for example Node from a CI tool cache outside its fixed PATH).
    call(
        "process.run",
        json!({
            "argv":["/bin/sh","-c", ". ./counter.sh; actual=$(increment 0); if [ \"$actual\" != 1 ]; then printf '%s\\n' 'increment(0) must be 1' >&2; exit 1; fi"],
            "verification":{"criteria":["Public increment advances the count"],"paths":["counter.sh"]}
        }),
    )
}

fn cycle(request: &ModelRequest) -> Option<Value> {
    request
        .messages
        .iter()
        .filter_map(|m| serde_json::from_str::<Value>(&m.content).ok())
        .find(|v| v["event"] == "coding_task_state")
        .map(|v| v["data"]["work_cycle"].clone())
}

fn sandbox() -> runtime::Sandbox {
    let mut sandbox = runtime::Sandbox::default();
    if let Some(path) = std::env::var_os("WERK_WORKER_BWRAP") {
        sandbox.bubblewrap = path.into();
    }
    sandbox
}

async fn fixture(
    turns: Vec<ScriptedTurn>,
    path: &std::path::Path,
) -> (WorkerEngine, SessionHandle, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider::new(turns));
    let engine = WorkerEngine::builder(provider.clone())
        .sandbox(sandbox())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "coding-basic".into(),
                prompt: "Implement and verify the requested behavior".into(),
                tools: [
                    "files.write",
                    "files.edit",
                    "files.read",
                    "files.search",
                    "task.plan",
                    "process.run",
                    "interaction.ask",
                ]
                .map(str::to_owned)
                .to_vec(),
                skills: vec![],
            },
            path,
            HostPolicy {
                workspace_writable: true,
                tool_permissions: [
                    "files.write",
                    "files.edit",
                    "files.read",
                    "files.search",
                    "process.run",
                ]
                .map(|n| (n.into(), ToolPermission::Allow))
                .into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    (engine, session, provider)
}

#[tokio::test]
async fn wandering_reads_are_redirected_then_real_review_and_repair_finish() {
    if let Err(error) = sandbox().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let initial = "increment() { printf '%s\\n' \"$1\"; }\n";
    let repaired = "increment() { printf '%s\\n' \"$(($1 + 1))\"; }\n";
    let mut turns = vec![
        plan("in_progress"),
        call(
            "files.write",
            json!({"path":"counter.sh","content":initial}),
        ),
    ];
    // Successful, differently spelled shell reads bypass exact-call and failed-
    // command guards. A plan update also isn't implementation progress.
    for n in 0..5 {
        turns.push(call(
            "process.run",
            json!({"argv":["/bin/sh","-c",format!("cat counter.sh # inspection {n}")]}),
        ));
    }
    turns.extend([
        plan("in_progress"),
        verify(),
        call(
            "files.edit",
            json!({"path":"counter.sh","expected_hash":hash(initial),"old":initial,"new":repaired}),
        ),
        verify(),
        plan("completed"),
        ScriptedTurn::text("Implemented and verified increment"),
    ]);
    let (engine, session, provider) = fixture(turns, dir.path()).await;
    let run = session
        .submit(
            "Build a counter whose public increment increases it by one",
            "start",
            Limits::continuous(),
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    let recorded = run.status().await.unwrap();
    let checks: Vec<_> = recorded
        .invocations
        .iter()
        .filter(|i| i.call.arguments.get("verification").is_some())
        .map(|i| &i.result)
        .collect();
    assert_eq!(
        result.status,
        RunStatus::Completed,
        "{:?}; verification results: {checks:#?}",
        result.error
    );
    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0].as_ref().unwrap().data["exit_code"], 1);
    assert_eq!(
        checks[0].as_ref().unwrap().data["verification"]["passed"],
        false
    );
    assert_eq!(checks[1].as_ref().unwrap().data["exit_code"], 0);
    assert_eq!(
        checks[1].as_ref().unwrap().data["verification"]["passed"],
        true
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("counter.sh")).unwrap(),
        repaired
    );
    let requests = provider.requests();
    let redirected = &requests[8];
    assert_eq!(cycle(redirected).unwrap()["refocus"], true);
    assert_eq!(cycle(redirected).unwrap()["changes_since_check"], 1);
    assert_eq!(cycle(redirected).unwrap()["check_due"], true);
    assert!(
        redirected
            .tools
            .iter()
            .all(|t| matches!(t.name.as_str(), "process.run" | "interaction.ask"))
    );
    assert!(
        redirected
            .messages
            .iter()
            .filter(|m| !m.tool_calls.is_empty())
            .count()
            <= 2
    );
    assert!(
        redirected
            .messages
            .iter()
            .any(|m| m.content.contains("older investigation groups omitted"))
    );
    validate_messages(&redirected.messages).unwrap();
    let steering = redirected.messages.last().unwrap();
    assert_eq!(steering.role, Role::User);
    assert!(steering.content.contains("Runtime work-cycle recovery"));
    assert!(steering.content.contains("behavioral smoke check"));
    let failed = cycle(&requests[9]).unwrap();
    assert_eq!(failed["suggested_phase"], "repair");
    assert!(
        failed["latest_check"]["failure_excerpt"]
            .as_str()
            .unwrap()
            .contains("increment(0) must be 1")
    );
    assert_eq!(cycle(&requests[10]).unwrap()["suggested_phase"], "build");
    assert_eq!(cycle(&requests[11]).unwrap()["suggested_phase"], "review");
    assert_eq!(cycle(&requests[11]).unwrap()["refocus"], false);
    assert!(
        requests[11]
            .messages
            .iter()
            .all(|m| !m.content.contains("Runtime work-cycle recovery"))
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn checkpoint_and_goal_survive_missing_transcript_and_new_user_steering() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, session, _) = fixture(
        vec![
            plan("in_progress"),
            call(
                "files.write",
                json!({"path":"counter.sh","content":"increment() { printf '%s\\n' \"$1\"; }\n"}),
            ),
        ],
        dir.path(),
    )
    .await;
    session
        .submit(
            "Build a public counter API",
            "start",
            Limits {
                max_steps: 2,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    let saved = session.snapshot().await.unwrap();
    let mut restored: SessionData =
        serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
    // Simulate removal of every tool transcript. Durable invocation receipts
    // and the task plan still exist, as with rolling compaction/resume.
    restored.messages = vec![
        Message::text(Role::User, "Build a public counter API"),
        Message::text(Role::User, "Also handle negative input"),
    ];
    let run = restored.runs.last_mut().unwrap();
    run.history_start = 0;
    run.contexts.clear();
    run.steering.push("Also handle negative input".into());
    let (request, _) = DefaultContextBuilder
        .build(&restored, restored.runs.last().unwrap(), vec![])
        .await
        .unwrap();
    validate_messages(&request.messages).unwrap();
    let progress = cycle(&request).unwrap();
    assert_eq!(progress["objective_excerpt"], "Build a public counter API");
    assert_eq!(
        progress["latest_user_update_excerpt"],
        "Also handle negative input"
    );
    assert_eq!(
        progress["current_milestone"],
        "Runnable counter with a verified increment"
    );
    assert_eq!(progress["last_change"]["path"], "counter.sh");
    let evidence = request
        .messages
        .iter()
        .find(|m| m.content.contains("coding_task_state"))
        .unwrap();
    assert!(
        evidence
            .content
            .contains("Implement increment, then assert increment 0 produces 1")
    );
    assert!(evidence.content.contains("counter.sh defines increment"));
    assert_eq!(evidence.role, Role::Assistant); // Checkpoint data gains no system authority.
    engine.close().await.unwrap();
}

#[tokio::test]
async fn investigation_before_any_implementation_does_not_force_edits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("input.txt"), "a\nb\nc\nd\ne\nf\ng\nh\n").unwrap();
    let mut turns = vec![plan("in_progress")];
    turns.extend((1..=8).map(|n| {
        call(
            "files.read",
            json!({"path":"input.txt","start_line":n,"end_line":n}),
        )
    }));
    let (engine, session, provider) = fixture(turns, dir.path()).await;
    session
        .submit(
            "Investigate this data only",
            "start",
            Limits {
                max_steps: 9,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(
        provider
            .requests()
            .iter()
            .filter_map(cycle)
            .all(|v| v["refocus"] == false)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("input.txt")).unwrap(),
        "a\nb\nc\nd\ne\nf\ng\nh\n"
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn repeated_initial_reads_restore_context_without_forcing_analysis_to_edit() {
    if let Err(error) = sandbox().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    for implement in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let before =
            "# Public counter\nincrement() { printf '%s\\n' \"$1\"; }\n# End of implementation\n";
        let after = before.replace("\"$1\"", "\"$(($1 + 1))\"");
        std::fs::write(dir.path().join("counter.sh"), before).unwrap();
        std::fs::write(dir.path().join("details.txt"), "One newly inspected fact").unwrap();
        std::fs::write(
            dir.path().join("notes.txt"),
            "Public contract: increment 0 must produce 1",
        )
        .unwrap();
        let mut turns = vec![
            call("files.read", json!({"path":"counter.sh"})),
            call("files.read", json!({"path":"notes.txt"})),
            call(
                "files.read",
                json!({"path":"counter.sh","start_line":1,"end_line":1}),
            ),
            call("files.read", json!({"path":"notes.txt"})),
            call("files.read", json!({"path":"counter.sh","start_line":3})),
            call("files.read", json!({"path":"details.txt"})),
        ];
        if implement {
            turns.extend([
                call("files.edit", json!({"path":"counter.sh","expected_hash":hash(before),"old":before,"new":before})),
                call("files.edit", json!({"path":"counter.sh","expected_hash":hash(before),"old":before,"new":after})),
                call("process.run", json!({"argv":["/bin/sh","-n","counter.sh"],
                    "verification":{"criteria":["Syntax is valid"],"paths":["counter.sh"]}})),
                verify(),
            ]);
        }
        turns.push(ScriptedTurn::text(if implement {
            "Increment implemented and verified"
        } else {
            "The current increment returns its input unchanged; no edits made"
        }));
        let (engine, session, provider) = fixture(turns, dir.path()).await;
        let goal = if implement {
            "Repair the existing counter so increment increases its input by one"
        } else {
            "Analyze the counter only. Do not modify any files"
        };
        let run = session
            .submit(goal, "start", Limits::continuous(), None)
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
        let requests = provider.requests();
        for index in if implement { 5..=7 } else { 5..=6 } {
            assert!(
                requests[index]
                    .messages
                    .last()
                    .unwrap()
                    .content
                    .contains("Runtime work-cycle recovery"),
                "new information and no-op edits must not end recovery at decision {index}"
            );
        }
        if implement {
            assert!(
                requests[8]
                    .tools
                    .iter()
                    .any(|tool| tool.name == "process.run"
                        && tool.input_schema["x-werk-work-cycle"] == "check"),
                "the first actual recovery edit must transition to behavioral review"
            );
            assert!(
                requests[9]
                    .tools
                    .iter()
                    .any(|tool| tool.name == "process.run"
                        && tool.input_schema["x-werk-work-cycle"] == "check"),
                "a passing syntax check must not end recovery review"
            );
            assert!(
                !requests[10]
                    .messages
                    .last()
                    .unwrap()
                    .content
                    .contains("Runtime work-cycle recovery"),
                "a distinct check ends recovery and restores the normal work cycle"
            );
        }
        let recovery = &requests[5];
        let steering = recovery.messages.last().unwrap();
        assert_eq!(steering.role, Role::User);
        assert!(
            steering
                .content
                .contains("before the first implementation change")
        );
        assert!(steering.content.contains("analysis-only"));
        assert!(
            recovery
                .messages
                .iter()
                .any(|m| m.role == Role::User && m.content == goal)
        );
        assert!(
            recovery
                .messages
                .iter()
                .any(|m| m.content.contains("increment()"))
        );
        assert!(
            recovery
                .messages
                .iter()
                .any(|m| m.content.contains("Public contract:"))
        );
        assert!(
            recovery
                .messages
                .iter()
                .filter(|m| !m.tool_calls.is_empty())
                .count()
                <= 2
        );
        assert!(recovery.tools.iter().any(|t| t.name == "files.read"));
        assert_eq!(cycle(recovery).unwrap()["check_due"], false);
        validate_messages(&recovery.messages).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("counter.sh")).unwrap(),
            if implement { after } else { before.into() }
        );
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn repeated_small_edits_cannot_postpone_a_behavioral_check() {
    if let Err(error) = sandbox().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let initial = "increment() { printf '%s\\n' \"$1\"; }\n";
    let increment = "increment() { printf '%s\\n' \"$(($1 + 1))\"; }\n";
    let with_default = "increment() { printf '%s\\n' \"$((${1:-0} + 1))\"; }\n";
    let mut turns = vec![call(
        "files.write",
        json!({"path":"counter.sh","content":initial}),
    )];
    for (old, new) in [(initial, increment), (increment, with_default)] {
        turns.extend((0..2).map(|_| call("files.read", json!({"path":"counter.sh"}))));
        turns.push(call(
            "files.edit",
            json!({"path":"counter.sh","expected_hash":hash(old),"old":old,"new":new}),
        ));
    }
    turns.extend((0..2).map(|_| call("files.read", json!({"path":"counter.sh"}))));
    // A no-op proposal and another genuine edit must not erase the need to
    // exercise the accumulated changes. Neither is a behavioral check.
    turns.push(call("files.edit", json!({"path":"counter.sh","expected_hash":hash(with_default),"old":with_default,"new":with_default})));
    let final_code = format!(
        "{with_default}increment_twice() {{ increment \"$(increment \"${{1:-0}}\")\"; }}\n"
    );
    turns.push(call("files.edit", json!({"path":"counter.sh","expected_hash":hash(with_default),"old":with_default,"new":final_code})));
    // Metadata cannot turn a successful source dump into a behavioral check.
    turns.push(call("process.run", json!({"argv":["cat","counter.sh"],"verification":{"criteria":["Read the public increment function"],"paths":["counter.sh"]}})));
    turns.push(call("process.run", json!({"argv":["/bin/sh","-n","counter.sh"],"verification":{"criteria":["Syntax is valid"],"paths":["counter.sh"]}})));
    turns.push(ScriptedTurn::text("The source was read successfully; done"));
    turns.extend([verify(), ScriptedTurn::text("Implemented and verified")]);
    let (engine, session, provider) = fixture(turns, dir.path()).await;
    let run = session
        .submit(
            "Implement a public increment function",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    let requests = provider.requests();
    assert_eq!(cycle(&requests[8]).unwrap()["check_due"], false);
    for request in &requests[9..=14] {
        let state = cycle(request).unwrap();
        assert_eq!(state["suggested_phase"], "check");
        assert_eq!(state["check_due"], true);
        assert!(
            request
                .messages
                .last()
                .unwrap()
                .content
                .contains("do not clear this checkpoint")
        );
        assert_eq!(request.tools.len(), 2);
        assert!(request.tools.iter().any(|t| t.name == "interaction.ask"));
        let tool = request
            .tools
            .iter()
            .find(|t| t.name == "process.run")
            .unwrap();
        assert!(
            tool.input_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "verification")
        );
    }
    assert_eq!(cycle(&requests[15]).unwrap()["check_due"], false);
    assert_eq!(cycle(&requests[15]).unwrap()["suggested_phase"], "review");
    assert!(requests[15].tools.iter().any(|t| t.name == "files.edit"));
    let snapshot = session.snapshot().await.unwrap();
    let dump = snapshot.run(&run.id).unwrap().invocations[11]
        .result
        .as_ref()
        .unwrap();
    assert_eq!(dump.data["exit_code"], 0);
    assert_eq!(dump.data["stdout"], with_default);
    assert!(dump.data["verification"].is_null());
    assert!(dump.data["verification_rejected"].as_str().is_some());
    for invocation in &snapshot.run(&run.id).unwrap().invocations[9..=10] {
        assert_eq!(
            invocation.result.as_ref().unwrap().error_code.as_deref(),
            Some("work_cycle_check_required")
        );
        assert!(invocation.approval.is_none());
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("counter.sh")).unwrap(),
        with_default
    );
    let mut denied = snapshot;
    denied.run_mut(&run.id).unwrap().invocations.truncate(9);
    denied.policy.hard_denied_tools.push("process.run".into());
    let (request, _) = DefaultContextBuilder
        .build(
            &denied,
            denied.run(&run.id).unwrap(),
            engine
                .tools()
                .into_iter()
                .filter(|t| t.name != "process.run")
                .collect(),
        )
        .await
        .unwrap();
    assert!(request.tools.iter().any(|t| t.name == "files.read"));
    assert!(request.tools.iter().any(|t| t.name == "files.edit"));
    engine.close().await.unwrap();
}

#[tokio::test]
async fn check_phase_refusals_keep_the_existing_repetition_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut turns = Vec::new();
    for n in 0..3 {
        let path = format!("part-{n}.sh");
        turns.push(call(
            "files.write",
            json!({"path":path,"content":format!("value={n}\n")}),
        ));
        turns.extend((0..2).map(|_| call("files.read", json!({"path":path}))));
    }
    turns.extend((0..4).map(|_| {
        call(
            "files.search",
            json!({"path":".","query":"placeholder__resume"}),
        )
    }));
    let (engine, session, _) = fixture(turns, dir.path()).await;
    let run = session
        .submit("Build and verify", "start", Limits::continuous(), None)
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().code, "repetition_limit");
    let saved = session.snapshot().await.unwrap();
    let invocations = &saved.run(&run.id).unwrap().invocations;
    assert_eq!(invocations.len(), 13);
    assert!(
        invocations[9..]
            .iter()
            .all(|i| i.result.as_ref().unwrap().error_code.as_deref()
                == Some("work_cycle_check_required"))
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn unknown_context_limit_recovers_once_from_checkpoint_with_smaller_input() {
    if let Err(error) = sandbox().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    for reject_again in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut turns = vec![call(
            "files.write",
            json!({"path":"counter.sh","content":"increment() { printf '%s\\n' \"$(($1 + 1))\"; }\n"}),
        )];
        turns.extend((0..3).map(|_| read_counter()));
        for turn in &mut turns {
            turn.deltas[0] = ModelDelta::Text {
                text: "Earlier investigation narrative. ".repeat(150),
            };
        }
        let rejected = || ScriptedTurn {
            deltas: vec![],
            delay_ms: 0,
            error: Some(WorkerError::new(
                "context_overflow",
                "No verified smaller server limit",
            )),
        };
        turns.push(rejected());
        if reject_again {
            turns.push(rejected());
        } else {
            turns.extend([verify(), ScriptedTurn::text("Implemented and verified")]);
        }
        let (engine, session, provider) = fixture(turns, dir.path()).await;
        let run = session
            .submit(
                "Implement increment. Preserve the public interface.",
                "start",
                Limits::default(),
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        if reject_again {
            assert_eq!(result.error.unwrap().code, "context_overflow");
            assert_eq!(provider.requests().len(), 6);
        } else {
            assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
        }
        let snapshot = session.snapshot().await.unwrap();
        let contexts = &snapshot.run(&run.id).unwrap().contexts;
        assert!(contexts[5].estimated_tokens < contexts[4].estimated_tokens);
        let requests = provider.requests();
        assert!(
            requests[5].messages.iter().any(
                |m| m.role == Role::User && m.content.contains("Preserve the public interface")
            )
        );
        assert!(
            requests[5]
                .messages
                .iter()
                .filter(|m| !m.tool_calls.is_empty())
                .count()
                <= 2
        );
        validate_messages(&requests[5].messages).unwrap();
        engine.close().await.unwrap();
    }
}

fn read_counter() -> ScriptedTurn {
    call("files.read", json!({"path":"counter.sh"}))
}

#[tokio::test]
async fn failed_review_bounds_investigation_then_repairs_and_finishes() {
    if let Err(error) = sandbox().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let initial = "increment() { printf '%s\\n' \"$1\"; }\n";
    let repaired = "increment() { printf '%s\\n' \"$(($1 + 1))\"; }\n";
    let (engine, session, provider) = fixture(vec![
        call("files.write", json!({"path":"counter.sh","content":initial})),
        verify(),
        call("files.read", json!({"path":"counter.sh"})),
        call("files.search", json!({"path":"counter.sh","query":"increment"})),
        // A proposed third investigation is deferred, never executed.
        call("files.read", json!({"path":"counter.sh","start_line":1,"end_line":1})),
        // A stale anchor must reopen diagnosis, not trap the model in edits.
        call("files.edit", json!({"path":"counter.sh","expected_hash":hash("stale"),"old":initial,"new":repaired})),
        call("files.read", json!({"path":"counter.sh"})),
        call("files.edit", json!({"path":"counter.sh","expected_hash":hash(initial),"old":initial,"new":repaired})),
        verify(),
        ScriptedTurn::text("Public increment repaired and checked"),
    ], dir.path()).await;
    let run = session
        .submit(
            "Build and check a public increment function",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    let requests = provider.requests();
    assert_eq!(cycle(&requests[4]).unwrap()["action_due"], true);
    assert!(requests[4].tools.iter().any(|t| t.name == "files.edit"));
    assert!(
        !requests[4]
            .tools
            .iter()
            .any(|t| matches!(t.name.as_str(), "files.read" | "files.search"))
    );
    assert!(requests[6].tools.iter().any(|t| t.name == "files.read"));
    assert_eq!(cycle(&requests[8]).unwrap()["check_due"], true);
    assert_eq!(cycle(&requests[9]).unwrap()["suggested_phase"], "review");
    let saved = session.snapshot().await.unwrap();
    let rejected = saved.run(&run.id).unwrap().invocations[4]
        .result
        .as_ref()
        .unwrap();
    assert_eq!(
        rejected.error_code.as_deref(),
        Some("work_cycle_check_required")
    );
    assert!(rejected.data["content"].is_null());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("counter.sh")).unwrap(),
        repaired
    );

    // Browser verification must remain available during the check phase,
    // including when process execution is not requestable.
    let mut checkpoint = saved;
    checkpoint.run_mut(&run.id).unwrap().invocations.truncate(8);
    checkpoint
        .policy
        .tool_permissions
        .insert("browser.check".into(), ToolPermission::Allow);
    checkpoint
        .policy
        .hard_denied_tools
        .push("process.run".into());
    let (request, _) = DefaultContextBuilder
        .build(
            &checkpoint,
            checkpoint.run(&run.id).unwrap(),
            engine
                .tools()
                .into_iter()
                .filter(|t| {
                    matches!(
                        t.name.as_str(),
                        "browser.check" | "files.read" | "interaction.ask"
                    )
                })
                .collect(),
        )
        .await
        .unwrap();
    assert!(request.tools.iter().any(|t| t.name == "browser.check"));
    assert!(!request.tools.iter().any(|t| t.name == "files.read"));
    engine.close().await.unwrap();
}

#[tokio::test]
async fn passing_receipt_without_recovery_action_does_not_reopen_exploration() {
    if let Err(error) = sandbox().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let initial = "increment() { printf '%s\\n' \"$1\"; }\n";
    let repaired = "increment() { printf '%s\\n' \"$(($1 + 1))\"; }\n";
    let mut turns = vec![call(
        "files.write",
        json!({"path":"counter.sh","content":initial}),
    )];
    turns.extend((0..4).map(|_| call("files.read", json!({"path":"counter.sh"}))));
    turns.extend([
        // A generic process can exit zero without establishing the task's behavior.
        call("process.run", json!({"argv":["/bin/sh","-c","printf 'looked at the source\\n'"],"verification":{"criteria":["Claimed check"],"paths":["counter.sh"]}})),
        call("files.edit", json!({"path":"counter.sh","expected_hash":hash(initial),"old":initial,"new":repaired})),
        verify(), ScriptedTurn::text("Increment verified"),
    ]);
    let (engine, session, provider) = fixture(turns, dir.path()).await;
    let run = session
        .submit(
            "Implement public increment",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    let requests = provider.requests();
    assert_eq!(cycle(&requests[6]).unwrap()["action_due"], true);
    assert!(!requests[6].tools.iter().any(|t| t.name == "files.search"));
    assert_eq!(cycle(&requests[7]).unwrap()["check_due"], true);
    assert_eq!(cycle(&requests[8]).unwrap()["action_due"], false);
    assert!(requests[8].tools.iter().any(|t| t.name == "files.read"));
    engine.close().await.unwrap();
}

#[tokio::test]
async fn recovery_can_replace_a_complete_version_without_copying_the_old_anchor() {
    if let Err(error) = sandbox().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let initial = "increment() { printf '%s\\n' \"$1\"; }\n";
    let repaired = "increment() { printf '%s\\n' \"$(($1 + 1))\"; }\n";
    let mut turns = vec![call(
        "files.write",
        json!({"path":"counter.sh","content":initial}),
    )];
    turns.push(verify());
    turns.extend((0..2).map(|_| call("files.read", json!({"path":"counter.sh"}))));
    turns.extend([
        call(
            "files.write",
            json!({"path":"counter.sh","expected_hash":hash("stale"),"content":repaired}),
        ),
        call("files.read", json!({"path":"counter.sh"})),
        call(
            "files.write",
            json!({"path":"counter.sh","expected_hash":hash(initial),"content":repaired}),
        ),
        verify(),
        ScriptedTurn::text("Completed and checked the replacement"),
    ]);
    let (engine, session, provider) = fixture(turns, dir.path()).await;
    let run = session
        .submit(
            "Implement public increment",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    let trace = run
        .status()
        .await
        .unwrap()
        .invocations
        .iter()
        .map(|i| (i.call.name.clone(), i.result.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        result.status,
        RunStatus::Completed,
        "{:?}; {trace:?}",
        result.error
    );
    let requests = provider.requests();
    let replacement = requests[4]
        .tools
        .iter()
        .find(|t| t.name == "files.write")
        .unwrap();
    assert_eq!(
        replacement.input_schema["properties"]["expected_hash"]["type"],
        "string"
    );
    assert!(requests[5].tools.iter().any(|t| t.name == "files.read"));
    assert_eq!(cycle(&requests[7]).unwrap()["check_due"], true);
    let snapshot = session.snapshot().await.unwrap();
    let calls = &snapshot.run(&run.id).unwrap().invocations;
    assert_eq!(
        calls[4].result.as_ref().unwrap().error_code.as_deref(),
        Some("write_conflict")
    );
    assert_eq!(
        calls[5].result.as_ref().unwrap().data["content"],
        initial.trim_end()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("counter.sh")).unwrap(),
        repaired
    );
    engine.close().await.unwrap();
}
