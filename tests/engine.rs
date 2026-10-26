use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use werk_worker::{
    context::validate_messages, policy::*, profiles::AgentProfile, providers::*, sessions::*, *,
};

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id("call"),
        name: name.into(),
        arguments: args,
    }
}
fn profile(names: &[&str]) -> AgentProfile {
    AgentProfile {
        schema_version: 1,
        name: "test".into(),
        prompt: "Test harness".into(),
        tools: names.iter().map(|s| s.to_string()).collect(),
        skills: vec![],
    }
}
fn grant(tool: &str, preapproved: bool) -> CapabilityGrant {
    CapabilityGrant {
        tool: tool.into(),
        version: "1".into(),
        resource: ".".into(),
        arguments_hash: None,
        expires_at_ms: None,
        preapproved,
    }
}
// Guard against hung tests, not production deadlines. Match approval()'s
// allowance for filesystem/SQLite writeback under parallel CI load.
async fn finish(run: &RunHandle) -> RunResult {
    tokio::time::timeout(std::time::Duration::from_secs(30), run.wait())
        .await
        .expect("run stalled")
        .unwrap()
}
async fn approval(run: &RunHandle) -> ApprovalRequest {
    let mut events = run.events(0);
    // This is a test-hang guard, not the worker's approval/work deadline.
    // Cold CI builds can leave enough filesystem writeback to delay SQLite's
    // durable approval event beyond five seconds. Deadline behavior is tested
    // separately with explicit run limits below.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(event) = events.next().await {
            if let EventKind::ApprovalRequested { request } = event.unwrap().event {
                return request;
            }
        }
        panic!("approval not emitted")
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn approval_time_is_paused_but_work_after_approval_still_has_a_deadline() {
    // Real filesystem work competes with the other all-targets tests. Keep
    // enough active headroom for it, while waiting beyond that budget for
    // approval and deliberately stalling much longer afterwards.
    const WORK_MS: u64 = 2_000;
    const APPROVAL_MS: u64 = WORK_MS + 100;
    for stall_after_approval in [false, true] {
        let workspace = TempDir::new().unwrap();
        let mut next = ScriptedTurn::text("Done");
        if stall_after_approval {
            next.delay_ms = 30_000;
        }
        let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![
            ScriptedTurn::calls(
                "",
                vec![call(
                    "files.write",
                    json!({"path":"saved.txt","content":"saved"}),
                )],
            ),
            next,
        ])))
        .build()
        .await
        .unwrap();
        let session = engine
            .create_session(
                profile(&["files.write"]),
                workspace.path(),
                HostPolicy {
                    workspace_writable: true,
                    grants: vec![grant("files.write", false)],
                    ..Default::default()
                },
                Value::Null,
            )
            .await
            .unwrap();
        let run = session
            .submit(
                "Write",
                "start",
                Limits {
                    max_wall_ms: WORK_MS,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let request = approval(&run).await;
        let old_deadline = run.status().await.unwrap().deadline_ms;
        tokio::time::sleep(std::time::Duration::from_millis(APPROVAL_MS)).await;
        assert_eq!(
            run.status().await.unwrap().status,
            RunStatus::WaitingApproval
        );
        run.respond_to_approval(&request.request_id, ApprovalDecision::AllowOnce, "allow")
            .await
            .unwrap();
        let result = finish(&run).await;
        if stall_after_approval {
            assert_eq!(result.status, RunStatus::Failed);
            assert_eq!(result.error.unwrap().code, "wall_budget");
        } else {
            assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
        }
        let state = run.status().await.unwrap();
        assert!(state.metrics.approval_wait_ms >= APPROVAL_MS);
        assert_eq!(
            state.deadline_ms - old_deadline,
            state.metrics.approval_wait_ms
        );
        assert!(state.approval_wait_started_at_ms.is_none());
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("saved.txt")).unwrap(),
            "saved"
        );
        let deadline = state.deadline_ms;
        run.respond_to_approval(&request.request_id, ApprovalDecision::AllowOnce, "allow")
            .await
            .unwrap();
        assert_eq!(
            run.status().await.unwrap().deadline_ms,
            deadline,
            "duplicate decisions must not credit wait time twice"
        );
        engine.close().await.unwrap();
    }
}
#[tokio::test]
async fn invalid_file_paths_explain_the_path_error_without_claiming_missing_permissions() {
    let dir = TempDir::new().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::calls(
            "",
            vec![call("files.read", json!({"path":"/workspace/../secret"}))],
        ),
        ScriptedTurn::text("Correct the path"),
    ]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let session = engine
        .create_session(
            profile(&["files.read"]),
            dir.path(),
            HostPolicy {
                grants: vec![grant("files.read", true)],
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("read", "start", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(finish(&run).await.status, RunStatus::Completed);
    let data = session.snapshot().await.unwrap();
    let result = data.runs[0].invocations[0].result.as_ref().unwrap();
    assert_eq!(result.status, Outcome::ValidationError);
    assert_eq!(result.error_code.as_deref(), Some("path_denied"));
    assert!(
        result.data["message"]
            .as_str()
            .unwrap()
            .contains("relative workspace path")
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn sandbox_file_alias_requests_approval_for_the_prepared_relative_path() {
    let dir = TempDir::new().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::calls(
            "",
            vec![call(
                "files.write",
                json!({"path":"/workspace/result.txt", "content":"approved"}),
            )],
        ),
        ScriptedTurn::text("done"),
    ]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let session = engine
        .create_session(
            profile(&["files.write"]),
            dir.path(),
            HostPolicy {
                workspace_writable: true,
                grants: vec![grant("files.write", false)],
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("write", "start", Limits::default(), None)
        .await
        .unwrap();
    let request = approval(&run).await;
    assert_eq!(request.description["arguments"]["path"], "result.txt");
    assert!(!dir.path().join("result.txt").exists());
    run.respond_to_approval(&request.request_id, ApprovalDecision::AllowOnce, "approve")
        .await
        .unwrap();
    assert_eq!(finish(&run).await.status, RunStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
        "approved"
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn text_submit_dedupe_and_command_conflict() {
    let dir = TempDir::new().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text("hello")]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(profile(&[]), dir.path(), HostPolicy::default(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("hi", "c1", Limits::default(), None)
        .await
        .unwrap();
    let repeated = session
        .submit("hi", "c1", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.id, repeated.id);
    assert!(matches!(repeated.receipt.status, ReceiptStatus::Duplicate));
    assert_eq!(
        session
            .submit("other", "c1", Limits::default(), None)
            .await
            .err()
            .unwrap()
            .code,
        "command_conflict"
    );
    assert_eq!(finish(&run).await.text.as_deref(), Some("hello"));
    assert_eq!(provider.requests().len(), 1);
    engine.close().await.unwrap();
}
#[tokio::test]
async fn tool_batch_validation_and_pairs_are_deterministic() {
    let dir = TempDir::new().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::calls(
            "working",
            vec![
                call("task.plan", json!({"steps":[]})),
                call("missing", json!({})),
                call("task.plan", json!({"steps":"bad"})),
            ],
        ),
        ScriptedTurn::text("done"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(&["task.plan"]),
            dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("work", "c", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(finish(&run).await.status, RunStatus::Completed);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    validate_messages(&requests[1].messages).unwrap();
    let results: Vec<_> = requests[1]
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| serde_json::from_str::<ToolResult>(&m.content).unwrap())
        .collect();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].status, Outcome::Succeeded);
    assert_eq!(results[1].error_code.as_deref(), Some("unknown_tool"));
    assert_eq!(results[2].status, Outcome::ValidationError);
    engine.close().await.unwrap();
}
#[tokio::test]
async fn truncated_attempt_never_dispatches_and_retry_has_new_identity() {
    let dir = TempDir::new().unwrap();
    let mut broken = ScriptedTurn::calls("partial", vec![call("task.plan", json!({"steps":[]}))]);
    broken.deltas.truncate(2);
    broken.error = Some(WorkerError::transient("interrupted_stream"));
    let provider = Arc::new(ScriptedProvider::new(vec![
        broken,
        ScriptedTurn::text("complete"),
    ]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let session = engine
        .create_session(
            profile(&["task.plan"]),
            dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("work", "c", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(finish(&run).await.status, RunStatus::Completed);
    let state = run.status().await.unwrap();
    assert!(state.invocations.is_empty());
    assert_eq!(state.attempts.len(), 2);
    assert_ne!(state.attempts[0].attempt_id, state.attempts[1].attempt_id);
    assert_eq!(state.attempts[0].partial_text, "partial");
    assert_eq!(
        session
            .snapshot()
            .await
            .unwrap()
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count(),
        1
    );
    engine.close().await.unwrap();
}
#[tokio::test]
async fn ask_denial_has_no_write_and_valid_next_history() {
    let dir = TempDir::new().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::calls(
            "",
            vec![call(
                "files.write",
                json!({"path":"draft.txt","content":"draft"}),
            )],
        ),
        ScriptedTurn::text("denied"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let policy = HostPolicy {
        workspace_writable: true,
        grants: vec![grant("files.write", false)],
        ..Default::default()
    };
    let session = engine
        .create_session(profile(&["files.write"]), dir.path(), policy, Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("draft", "c", Limits::default(), None)
        .await
        .unwrap();
    let request = approval(&run).await;
    assert!(!dir.path().join("draft.txt").exists());
    assert_eq!(
        run.respond_to_approval("wrong", ApprovalDecision::AllowOnce, "bad")
            .await
            .unwrap_err()
            .code,
        "unknown_request"
    );
    run.respond_to_approval(&request.request_id, ApprovalDecision::Deny, "deny")
        .await
        .unwrap();
    assert_eq!(finish(&run).await.status, RunStatus::Completed);
    assert!(!dir.path().join("draft.txt").exists());
    validate_messages(&provider.requests()[1].messages).unwrap();
    assert_eq!(
        run.status().await.unwrap().invocations[0]
            .result
            .as_ref()
            .unwrap()
            .status,
        Outcome::Denied
    );
    engine.close().await.unwrap();
}
#[tokio::test]
async fn patch_approval_binds_base_and_conflict_preserves_user_change() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.txt"), "old").unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::calls(
            "",
            vec![call(
                "files.edit",
                json!({"path":"a.txt","old":"old","new":"new","expected_hash":hash("old")}),
            )],
        ),
        ScriptedTurn::text("conflict"),
    ]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let session = engine
        .create_session(
            profile(&["files.edit"]),
            dir.path(),
            HostPolicy {
                workspace_writable: true,
                grants: vec![grant("files.edit", false)],
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("edit", "c", Limits::default(), None)
        .await
        .unwrap();
    let request = approval(&run).await;
    assert_eq!(request.description["arguments"]["diff"]["after"], "new");
    std::fs::write(dir.path().join("a.txt"), "user change").unwrap();
    run.respond_to_approval(&request.request_id, ApprovalDecision::AllowOnce, "yes")
        .await
        .unwrap();
    finish(&run).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "user change"
    );
    assert_eq!(
        run.status().await.unwrap().invocations[0]
            .result
            .as_ref()
            .unwrap()
            .error_code
            .as_deref(),
        Some("write_conflict")
    );
    engine.close().await.unwrap();
}
#[tokio::test]
async fn interaction_steering_followup_and_busy() {
    for mode in [harness::HarnessMode::Classic, harness::HarnessMode::Minimal] {
        let dir = TempDir::new().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedTurn::calls(
                "",
                vec![call(
                    "interaction.ask",
                    json!({"question":"Which format?","choices":["JSON","text"]}),
                )],
            ),
            ScriptedTurn::text("first"),
            ScriptedTurn::text("second"),
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .harness(harness::HarnessConfig {
                mode,
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                profile(&["interaction.ask"]),
                dir.path(),
                HostPolicy::default(),
                Value::Null,
            )
            .await
            .unwrap();
        let run = session
            .submit("work", "first", Limits::default(), None)
            .await
            .unwrap();
        let mut events = run.events(0);
        let request = loop {
            if let EventKind::InteractionRequested { request } =
                events.next().await.unwrap().unwrap().event
            {
                break request;
            }
        };
        assert_eq!(
            session
                .submit("collision", "bad", Limits::default(), None)
                .await
                .err()
                .unwrap()
                .code,
            "busy"
        );
        run.steer("Keep citations", "steer").await.unwrap();
        let next = session
            .follow_up("then summarize", "follow", Limits::default())
            .await
            .unwrap();
        run.respond_to_interaction(&request.request_id, "JSON", "answer")
            .await
            .unwrap();
        assert_eq!(finish(&run).await.status, RunStatus::Completed);
        assert_eq!(finish(&next).await.status, RunStatus::Completed);
        assert!(
            provider.requests()[1]
                .messages
                .iter()
                .any(|m| m.content == "Keep citations")
        );
        engine.close().await.unwrap();
    }
}
#[tokio::test]
async fn cancellation_while_streaming_and_approval_does_not_continue() {
    for wait_for_approval in [false, true] {
        let dir = TempDir::new().unwrap();
        let turn = if wait_for_approval {
            ScriptedTurn::calls("", vec![call("files.read", json!({"path":"a"}))])
        } else {
            let mut t = ScriptedTurn::text("late");
            t.delay_ms = 200;
            t
        };
        let provider = Arc::new(ScriptedProvider::new(vec![turn]));
        let engine = WorkerEngine::builder(provider.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                profile(&["files.read"]),
                dir.path(),
                HostPolicy {
                    grants: vec![grant("files.read", false)],
                    ..Default::default()
                },
                Value::Null,
            )
            .await
            .unwrap();
        let run = session
            .submit("work", "c", Limits::default(), None)
            .await
            .unwrap();
        if wait_for_approval {
            let request = approval(&run).await;
            run.cancel("stop", "stop").await.unwrap();
            assert!(
                run.respond_to_approval(&request.request_id, ApprovalDecision::AllowOnce, "late")
                    .await
                    .is_err()
            );
        } else {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while provider.requests().is_empty() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            run.cancel("stop", "stop").await.unwrap();
        }
        assert_eq!(finish(&run).await.status, RunStatus::Cancelled);
        assert_eq!(provider.requests().len(), 1);
        validate_messages(&session.snapshot().await.unwrap().messages).unwrap();
        engine.close().await.unwrap();
    }
}
#[tokio::test]
async fn budgets_include_retries_and_output_correction() {
    let dir = TempDir::new().unwrap();
    let error = ScriptedTurn {
        deltas: vec![],
        delay_ms: 0,
        error: Some(WorkerError::transient("network_error")),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        error.clone(),
        error,
        ScriptedTurn::text("not allowed"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(profile(&[]), dir.path(), HostPolicy::default(), Value::Null)
        .await
        .unwrap();
    let limits = Limits {
        max_model_requests: 2,
        ..Default::default()
    };
    let run = session.submit("work", "c", limits, None).await.unwrap();
    let result = finish(&run).await;
    assert_eq!(result.error.unwrap().code, "request_budget");
    assert_eq!(provider.requests().len(), 2);
    engine.close().await.unwrap();
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::text("bad"),
        ScriptedTurn::text("{\"ok\":true}"),
    ])))
    .build()
    .await
    .unwrap();
    let session = engine
        .create_session(profile(&[]), dir.path(), HostPolicy::default(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit(
            "json",
            "c",
            Limits::default(),
            Some(json!({"type":"object","properties":{"ok":{"const":true}},"required":["ok"]})),
        )
        .await
        .unwrap();
    assert_eq!(finish(&run).await.status, RunStatus::Completed);
    assert_eq!(run.status().await.unwrap().metrics.model_requests, 2);
    engine.close().await.unwrap();
}
#[tokio::test]
async fn resumed_requests_can_be_answered_after_resume_with_the_original_ids() {
    async fn waiting(run: &RunHandle, expected: RunStatus) -> Run {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let state = run.status().await.unwrap();
                if state.status == expected {
                    return state;
                }
                assert!(!state.status.terminal(), "{:?}", state.error);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("recovered request never became answerable")
    }
    for (name, arguments, status) in [
        (
            "files.write",
            json!({"path":"new.txt","content":"saved"}),
            RunStatus::WaitingApproval,
        ),
        (
            "interaction.ask",
            json!({"question":"Which team?","choices":["Operations"]}),
            RunStatus::WaitingInput,
        ),
    ] {
        let workspace = TempDir::new().unwrap();
        let engine =
            WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ScriptedTurn::calls(
                "",
                vec![call(name, arguments)],
            )])))
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                profile(&[name]),
                workspace.path(),
                HostPolicy {
                    workspace_writable: true,
                    grants: vec![grant("files.write", false)],
                    ..Default::default()
                },
                Value::Null,
            )
            .await
            .unwrap();
        let run = session
            .submit("prepare request", "submit", Limits::default(), None)
            .await
            .unwrap();
        let before = waiting(&run, status).await;
        let mut captured = session.snapshot().await.unwrap();
        if status == RunStatus::WaitingApproval {
            // Simulate a durable approval surviving a minute of host downtime,
            // well beyond the original active deadline. No real sleep needed.
            let saved = captured.runs.iter_mut().find(|r| r.id == run.id).unwrap();
            saved.deadline_ms = now_ms() - 30_000;
            saved.approval_wait_started_at_ms = Some(now_ms() - 60_000);
        }
        let store = Arc::new(InMemoryStore::default());
        store.save(&captured, None).await.unwrap();
        engine.close().await.unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text("recovered")]));
        let fresh = WorkerEngine::builder(provider.clone())
            .store(store)
            .build()
            .await
            .unwrap();
        let resumed = fresh.resume_run(&captured.id, &run.id).await.unwrap();
        let after = waiting(&resumed, status).await;
        assert_eq!(after.invocations.len(), 1);
        assert_eq!(
            after.invocations[0].invocation_id,
            before.invocations[0].invocation_id
        );
        assert!(
            provider.requests().is_empty(),
            "Recovery must not regenerate the tool call"
        );
        if status == RunStatus::WaitingApproval {
            let original = before.invocations[0].approval.as_ref().unwrap();
            assert_eq!(
                after.invocations[0].approval.as_ref().unwrap().request_id,
                original.request_id
            );
            assert!(!workspace.path().join("new.txt").exists());
            resumed
                .respond_to_approval(&original.request_id, ApprovalDecision::AllowOnce, "allow")
                .await
                .unwrap();
        } else {
            let original = before.invocations[0].interaction.as_ref().unwrap();
            assert_eq!(
                after.invocations[0]
                    .interaction
                    .as_ref()
                    .unwrap()
                    .request_id,
                original.request_id
            );
            resumed
                .respond_to_interaction(&original.request_id, "Operations", "answer")
                .await
                .unwrap();
        }
        assert_eq!(finish(&resumed).await.status, RunStatus::Completed);
        if status == RunStatus::WaitingApproval {
            let state = resumed.status().await.unwrap();
            assert!(state.metrics.approval_wait_ms >= 60_000);
            assert!(state.approval_wait_started_at_ms.is_none());
            assert!(state.deadline_ms > now_ms());
        }
        assert_eq!(provider.requests().len(), 1);
        let data = fresh
            .open_session(&captured.id)
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap();
        assert_eq!(
            data.events
                .iter()
                .filter(|e| matches!(
                    e.event,
                    EventKind::ApprovalRequested { .. } | EventKind::InteractionRequested { .. }
                ))
                .count(),
            1
        );
        assert_eq!(
            data.events
                .iter()
                .filter(|e| matches!(e.event, EventKind::ToolCompleted { .. }))
                .count(),
            1
        );
        fresh.close().await.unwrap();
    }
}

#[tokio::test]
async fn sqlite_ownership_future_version_corruption_and_resume_approval() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("sessions.sqlite");
    let workspace = TempDir::new().unwrap();
    let store = Arc::new(SqliteStore::open(&path).await.unwrap());
    assert_eq!(
        SqliteStore::open(&path).await.err().unwrap().code,
        "store_owned"
    );
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ScriptedTurn::calls(
        "",
        vec![call(
            "files.write",
            json!({"path":"new.txt","content":"saved"}),
        )],
    )])))
    .store(store.clone())
    .build()
    .await
    .unwrap();
    let session = engine
        .create_session(
            profile(&["files.write"]),
            workspace.path(),
            HostPolicy {
                workspace_writable: true,
                grants: vec![grant("files.write", false)],
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("write", "c", Limits::default(), None)
        .await
        .unwrap();
    let request = approval(&run).await;
    let captured = session.snapshot().await.unwrap();
    // Independent database copy represents a process-loss durable boundary.
    let recovery_path = dir.path().join("recovery.sqlite");
    let recovery = Arc::new(SqliteStore::open(&recovery_path).await.unwrap());
    recovery.save(&captured, None).await.unwrap();
    engine.close().await.unwrap();
    let fresh = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
        "resumed",
    )])))
    .store(recovery)
    .build()
    .await
    .unwrap();
    let session = fresh.open_session(&captured.id).await.unwrap();
    assert_eq!(
        session
            .snapshot()
            .await
            .unwrap()
            .run(&run.id)
            .unwrap()
            .status,
        RunStatus::Interrupted
    );
    let handle = session.run(&run.id).await.unwrap();
    handle
        .respond_to_approval(&request.request_id, ApprovalDecision::AllowOnce, "decision")
        .await
        .unwrap();
    let resumed = fresh.resume_run(&captured.id, &run.id).await.unwrap();
    assert_eq!(finish(&resumed).await.status, RunStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("new.txt")).unwrap(),
        "saved"
    );
    fresh.close().await.unwrap();
    let future = dir.path().join("future.sqlite");
    let conn = rusqlite::Connection::open(&future).unwrap();
    conn.execute_batch("PRAGMA user_version=999;").unwrap();
    drop(conn);
    assert_eq!(
        SqliteStore::open(&future).await.err().unwrap().code,
        "store_version"
    );
    let corrupt = dir.path().join("corrupt.sqlite");
    std::fs::write(&corrupt, "not sqlite").unwrap();
    assert!(SqliteStore::open(corrupt).await.is_err());
}
#[tokio::test]
async fn identical_actions_reused_provider_ids_are_not_deduplicated() {
    let dir = TempDir::new().unwrap();
    let c = ToolCall {
        id: "same_provider_id".into(),
        name: "task.plan".into(),
        arguments: json!({"steps":[]}),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::calls("", vec![c.clone()]),
        ScriptedTurn::calls("", vec![c]),
        ScriptedTurn::text("done"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(&["task.plan"]),
            dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("twice", "c", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(finish(&run).await.status, RunStatus::Completed);
    let state = run.status().await.unwrap();
    assert_eq!(state.invocations.len(), 2);
    assert_ne!(
        state.invocations[0].invocation_id,
        state.invocations[1].invocation_id
    );
    validate_messages(&provider.requests()[2].messages).unwrap();
    engine.close().await.unwrap();
}
#[tokio::test]
async fn event_replay_and_fork_have_independent_identity_and_policy() {
    let dir = TempDir::new().unwrap();
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
        "done",
    )])))
    .build()
    .await
    .unwrap();
    let session = engine
        .create_session(profile(&[]), dir.path(), HostPolicy::default(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("work", "c", Limits::default(), None)
        .await
        .unwrap();
    finish(&run).await;
    let data = session.snapshot().await.unwrap();
    let cursor = data.events[2].cursor;
    let mut replay = session.events(cursor);
    let event = replay.next().await.unwrap().unwrap();
    assert_eq!(event.cursor, cursor + 1);
    let fork = session.fork(HostPolicy::default()).await.unwrap();
    let forked = fork.snapshot().await.unwrap();
    assert_ne!(data.id, forked.id);
    assert_ne!(data.sandbox_id, forked.sandbox_id);
    assert!(forked.runs.is_empty());
    assert!(forked.commands.is_empty());
    assert!(forked.policy.grants.is_empty());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn paused_event_subscriber_in_select_cannot_hold_session_lock() {
    for _ in 0..12 {
        let dir = TempDir::new().unwrap();
        let engine =
            WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
                "done",
            )])))
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(&[]), dir.path(), HostPolicy::default(), Value::Null)
            .await
            .unwrap();
        let run = session
            .submit("work", "c", Limits::default(), None)
            .await
            .unwrap();
        let mut events = run.events(0);
        loop {
            tokio::select! {result=run.wait()=>{assert_eq!(result.unwrap().status,RunStatus::Completed);break;},event=events.next()=>{if event.is_none(){break;}}}
        }
        // Keep the paused stream alive while taking the next snapshot/closing.
        tokio::time::timeout(std::time::Duration::from_secs(2), session.snapshot())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), engine.close())
            .await
            .unwrap()
            .unwrap();
        drop(events);
    }
}
