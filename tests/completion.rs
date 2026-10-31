use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{policy::*, profiles::AgentProfile, providers::*, *};

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
        json!({"steps":[{"id":"game","description":"Implement and verify real start behavior","status":status}]}),
    )
}
fn verify(expected: &str) -> ScriptedTurn {
    call(
        "process.run",
        json!({"argv":["/bin/sh","-c",format!("test \"$(cat game.txt)\" = {expected}")],"verification":{"kind":"behavior","criteria":["Fixture file matches expected state"],"paths":["game.txt"]}}),
    )
}
async fn engine(turns: Vec<ScriptedTurn>, dir: &std::path::Path) -> (WorkerEngine, SessionHandle) {
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(turns)))
        .build()
        .await
        .unwrap();
    let profile = AgentProfile {
        schema_version: 1,
        name: "coding-basic".into(),
        prompt: "Implement and verify".into(),
        tools: ["files.write", "files.edit", "process.run", "task.plan"]
            .map(str::to_owned)
            .to_vec(),
        skills: vec![],
    };
    let session = engine
        .create_session(
            profile,
            dir,
            HostPolicy {
                workspace_writable: true,
                tool_permissions: ["files.write", "files.edit", "process.run"]
                    .map(|name| (name.into(), ToolPermission::Allow))
                    .into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    (engine, session)
}
async fn sandbox() -> bool {
    if let Err(error) = runtime::Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        eprintln!("SKIP real sandbox: {error}");
        return false;
    }
    true
}

#[tokio::test]
async fn incomplete_implementation_and_failed_checks_are_repaired_before_completion() {
    if !sandbox().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (engine,session)=engine(vec![
        plan("in_progress"), call("files.write", json!({"path":"game.txt","content":"stub"})),
        ScriptedTurn::text("Done"), plan("completed"), ScriptedTurn::text("Done"),
        verify("playable"), ScriptedTurn::text("Done"),
        call("files.edit",json!({"path":"game.txt","expected_hash":hash("stub"),"old":"stub","new":"playable"})),
        verify("playable"), ScriptedTurn::text("Verified behavior"),
    ],dir.path()).await;
    let run = session
        .submit("Make the game work", "start", Limits::default(), None)
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(result.text.as_deref(), Some("Verified behavior"));
    let state = run.status().await.unwrap();
    let checks: Vec<_> = state
        .invocations
        .iter()
        .filter(|i| i.call.arguments.get("verification").is_some())
        .collect();
    assert_eq!(checks.len(), 2);
    assert_eq!(
        checks[0].result.as_ref().unwrap().data["verification"]["passed"],
        false
    );
    assert_eq!(
        checks[1].result.as_ref().unwrap().data["verification"]["passed"],
        true
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "playable"
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn unrelated_success_cannot_hide_an_unresolved_check_at_completion() {
    if !sandbox().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (engine, session) = engine(
        vec![
            call("files.write", json!({"path":"game.txt","content":"stub"})),
            verify("playable"),
            verify("stub"),
            ScriptedTurn::text("The unrelated check passed; done"),
            call("files.edit", json!({"path":"game.txt","expected_hash":hash("stub"),"old":"stub","new":"playable"})),
            verify("playable"),
            ScriptedTurn::text("The original failure is fixed and verified"),
        ],
        dir.path(),
    ).await;
    let result = session
        .submit(
            "Make the game playable",
            "start",
            Limits::continuous(),
            None,
        )
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(
        result.text.as_deref(),
        Some("The original failure is fixed and verified")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "playable"
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn edits_invalidate_previous_successful_verification() {
    if !sandbox().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (engine, session) = engine(
        vec![
            call("files.write", json!({"path":"game.txt","content":"one"})),
            verify("one"),
            call(
                "files.edit",
                json!({"path":"game.txt","expected_hash":hash("one"),"old":"one","new":"two"}),
            ),
            ScriptedTurn::text("Old test passed"),
            verify("two"),
            ScriptedTurn::text("Current test passed"),
        ],
        dir.path(),
    )
    .await;
    let result = session
        .submit("Update and verify", "start", Limits::default(), None)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(result.text.as_deref(), Some("Current test passed"));
    engine.close().await.unwrap();
}

#[tokio::test]
async fn repeated_unverified_finals_never_become_success() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, session) = engine(
        vec![
            call("files.write", json!({"path":"game.txt","content":"stub"})),
            ScriptedTurn::text("done"),
            ScriptedTurn::text("done"),
            ScriptedTurn::text("done"),
            ScriptedTurn::text("done"),
        ],
        dir.path(),
    )
    .await;
    let result = session
        .submit("Implement", "start", Limits::continuous(), None)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().code, "completion_stalled");
    assert!(result.text.is_none());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn continuous_runs_pass_the_old_step_and_tool_limits() {
    let dir = tempfile::tempdir().unwrap();
    let mut turns: Vec<_> = (0..70)
        .map(|i| {
            call(
                "files.write",
                json!({"path":format!("file{i}"),"content":"saved"}),
            )
        })
        .collect();
    turns.push(ScriptedTurn::text("Done"));
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(turns)))
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "fixture".into(),
                prompt: "Create files".into(),
                tools: vec!["files.write".into()],
                skills: vec![],
            },
            dir.path(),
            HostPolicy {
                workspace_writable: true,
                tool_permissions: [("files.write".into(), ToolPermission::Allow)].into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("Write 70 files", "start", Limits::continuous(), None)
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(result.metrics.model_requests, 71);
    assert_eq!(run.status().await.unwrap().deadline_ms, 0);
    assert!(dir.path().join("file69").exists());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn truncated_verification_keeps_durable_evidence_and_current_plan_in_context() {
    use werk_worker::context::{ContextBuilder, DefaultContextBuilder};
    if !sandbox().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (engine,session)=engine(vec![
        plan("completed"),
        call("files.write",json!({"path":"./game.txt","content":"verified"})),
        call("process.run",json!({"argv":["/bin/sh","-c","test $(cat game.txt) = verified && seq 1 4000"],"verification":{"kind":"behavior","criteria":["Fixture content is verified"],"paths":["/workspace/game.txt"]}})),
        ScriptedTurn::text("Verified"),
    ],dir.path()).await;
    let run = session
        .submit(
            "Implement and check",
            "start",
            Limits {
                max_output_bytes: 512,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let data = session.snapshot().await.unwrap();
    // Serialize/reload the same state representation used by durable stores.
    let mut saved: SessionData =
        serde_json::from_slice(&serde_json::to_vec(&data).unwrap()).unwrap();
    let state = saved.run(&run.id).unwrap();
    let check = state
        .invocations
        .iter()
        .find(|i| i.call.name == "process.run")
        .unwrap();
    assert!(check.result.as_ref().unwrap().truncated);
    assert!(
        serde_json::to_vec(&check.result.as_ref().unwrap().data)
            .unwrap()
            .len()
            <= 512
    );
    let receipt =
        &saved.metadata["_werkworker_harness"][&run.id]["verifications"][&check.invocation_id];
    assert_eq!(receipt["passed"], true);
    assert_eq!(receipt["files"]["game.txt"], hash("verified"));
    // Compaction may remove the complete tool transcript; the task state must
    // still expose the current plan and host-produced check result.
    saved.messages = vec![Message::text(Role::User, "Implement and check")];
    saved.run_mut(&run.id).unwrap().history_start = 0;
    let (request, _) = DefaultContextBuilder
        .build(&saved, saved.run(&run.id).unwrap(), vec![])
        .await
        .unwrap();
    let evidence = request
        .messages
        .iter()
        .find(|m| m.content.contains("coding_task_state"))
        .unwrap();
    assert_eq!(evidence.role, Role::Assistant);
    assert!(evidence.content.contains("Current run plan"));
    assert!(evidence.content.contains("Fixture content is verified"));
    assert!(evidence.content.contains(&hash("verified")));
    engine.close().await.unwrap();
}

#[tokio::test]
async fn a_check_that_changes_its_own_source_is_not_accepted() {
    if !sandbox().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (engine,session)=engine(vec![
        call("files.write",json!({"path":"game.txt","content":"stub"})),
        call("process.run",json!({"argv":["/bin/sh","-c","printf repaired > game.txt"],"verification":{"kind":"behavior","criteria":["State is correct"],"paths":["game.txt"]}})),
        ScriptedTurn::text("Done"),verify("repaired"),ScriptedTurn::text("Verified after repair"),
    ],dir.path()).await;
    let run = session
        .submit("Implement", "start", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(
        run.wait().await.unwrap().text.as_deref(),
        Some("Verified after repair")
    );
    let state = run.status().await.unwrap();
    let check = state
        .invocations
        .iter()
        .find(|i| i.call.name == "process.run")
        .unwrap();
    assert_eq!(
        check.result.as_ref().unwrap().data["verification"]["passed"],
        false
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn resumed_final_candidate_rechecks_current_files_before_acceptance() {
    use werk_worker::sessions::{SessionStore, SqliteStore};
    if !sandbox().await {
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let path = storage.path().join("sessions.sqlite");
    let (first, session) = engine(
        vec![
            call("files.write", json!({"path":"game.txt","content":"old"})),
            verify("old"),
            ScriptedTurn::text("Old candidate"),
        ],
        workspace.path(),
    )
    .await;
    let run = session
        .submit("Implement and check", "start", Limits::continuous(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let mut saved = session.snapshot().await.unwrap();
    first.close().await.unwrap();
    // Explicit interrupted-final fixture, not a simulated OS crash: retain
    // receipts/candidate but change the source between persistence and resume.
    saved.run_mut(&run.id).unwrap().status = RunStatus::Interrupted;
    let store = SqliteStore::open(&path).await.unwrap();
    store.save(&saved, None).await.unwrap();
    store.close().await.unwrap();
    std::fs::write(workspace.path().join("game.txt"), "current").unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        verify("current"),
        ScriptedTurn::text("Current candidate"),
    ]));
    let reopened = WorkerEngine::builder(provider)
        .store(Arc::new(SqliteStore::open(&path).await.unwrap()))
        .build()
        .await
        .unwrap();
    let resumed = reopened.resume_run(&saved.id, &run.id).await.unwrap();
    let result = resumed.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(result.text.as_deref(), Some("Current candidate"));
    assert_eq!(resumed.status().await.unwrap().deadline_ms, 0);
    reopened.close().await.unwrap();
}
