//! Offline replay of the observed edit -> deletion -> stale receipt -> rebuild loop.
#![cfg(target_os = "linux")]
use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{
    context::{ContextBuilder, DefaultContextBuilder},
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
fn read(path: &str) -> ScriptedTurn {
    call("files.read", json!({"path":path}))
}
fn edit(old: &str, new: &str) -> ScriptedTurn {
    call(
        "files.edit",
        json!({"path":"game.txt","expected_hash":hash("working"),"old":old,"new":new}),
    )
}
fn process(argv: &[&str]) -> ScriptedTurn {
    call("process.run", json!({"argv":argv}))
}
async fn sandbox() -> bool {
    if let Err(e) = runtime::Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{e}");
        return false;
    }
    true
}
async fn fixture(
    turns: Vec<ScriptedTurn>,
    dir: &std::path::Path,
) -> (WorkerEngine, SessionHandle, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider::new(turns));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let tools = [
        "files.read",
        "files.list",
        "files.write",
        "files.edit",
        "process.run",
        "task.plan",
    ];
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "coding-basic".into(),
                prompt: "Implement and verify".into(),
                tools: tools.map(str::to_owned).to_vec(),
                skills: vec![],
            },
            dir,
            HostPolicy {
                workspace_writable: true,
                tool_permissions: tools.map(|n| (n.into(), ToolPermission::Allow)).into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    (engine, session, provider)
}
fn task_state(request: &ModelRequest) -> Value {
    request
        .messages
        .iter()
        .rev()
        .filter_map(|m| serde_json::from_str::<Value>(&m.content).ok())
        .find(|v| v["event"] == "coding_task_state")
        .unwrap()["data"]
        .clone()
}

#[tokio::test]
async fn missing_observation_survives_compaction_resume_and_output_truncation() {
    if !sandbox().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "working").unwrap();
    let (engine, session, provider) = fixture(
        vec![
            read("./game.txt"),
            process(&["sh", "-c", "rm game.txt; seq 1 4000"]),
            read("/workspace/game.txt"),
            call("files.list", json!({"path":"."})),
        ],
        dir.path(),
    )
    .await;
    let run = session
        .submit(
            "Consolidate",
            "start",
            Limits {
                max_steps: 4,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
    assert_eq!(
        task_state(&provider.requests()[2])["files"][0]["status"],
        "missing",
        "process deletion must invalidate the hash before a reread, even with truncated stdout"
    );
    let mut data: SessionData =
        serde_json::from_slice(&serde_json::to_vec(&session.snapshot().await.unwrap()).unwrap())
            .unwrap();
    for _ in 0..16 {
        data.messages.push(Message::text(
            Role::Assistant,
            "unrelated old commentary ".repeat(120),
        ));
    }
    data.run_mut(&run.id).unwrap().limits.context_tokens = 12000;
    let (request, snapshot) = DefaultContextBuilder
        .build(&data, data.run(&run.id).unwrap(), vec![])
        .await
        .unwrap();
    assert!(!snapshot.omitted_messages.is_empty());
    let files = task_state(&request)["files"].clone();
    assert_eq!(files.as_array().unwrap().len(), 1);
    assert_eq!(files[0]["path"], "game.txt");
    assert_eq!(files[0]["status"], "missing");
    assert!(files[0].get("sha256").is_none());
    assert!(files[0].get("observed_tail_excerpt").is_none());
    for m in &request.messages {
        if let Ok(v) = serde_json::from_str::<Value>(&m.content)
            && v["event"] == "retained_evidence"
        {
            assert!(v["data"]["writes"].as_array().unwrap().is_empty());
            assert!(
                v["data"]["reads"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|r| r["status"] == "missing")
            );
        }
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn failed_edit_cannot_be_bypassed_by_shell_deletion_and_recovers_to_verified_work() {
    if !sandbox().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "working").unwrap();
    let (engine, session, provider) = fixture(
        vec![
            read("game.txt"),
            edit("invented anchor", "new"),
            process(&["sh", "-c", "rm game.txt"]),
            process(&[
                "python3",
                "-c",
                "from pathlib import Path; Path('game.txt').unlink()",
            ]),
            read("/workspace/game.txt"),
            edit("working", "repaired"),
            call(
                "process.run",
                json!({"argv":["sh","-c","test \"$(cat game.txt)\" = repaired"],
            "verification":{"criteria":["Game fixture is repaired"],"paths":["game.txt"]}}),
            ),
            ScriptedTurn::text("Verified"),
        ],
        dir.path(),
    )
    .await;
    let run = session
        .submit("Repair the game", "start", Limits::default(), None)
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    let data = session.snapshot().await.unwrap();
    let inv = &data.run(&run.id).unwrap().invocations;
    for n in [2, 3] {
        let r = inv[n].result.as_ref().unwrap();
        assert_ne!(r.data["exit_code"], 0);
        assert!(r.data["repair_protection"].is_string());
        assert_eq!(r.data["workspace_changed"], false);
    }
    assert_eq!(
        inv[4].result.as_ref().unwrap().data["sha256"],
        hash("working")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "repaired"
    );
    assert!(
        task_state(&provider.requests()[6])["pending_repairs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(inv[6].prepared["sandbox"]["workspace_writable"], true);
    engine.close().await.unwrap();
}

#[tokio::test]
async fn path_aliases_version_checks_and_noop_writes_do_not_reset_read_loop() {
    if !sandbox().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "working").unwrap();
    let (engine, session, _) = fixture(
        vec![
            read("game.txt"),
            process(&["python3", "--version"]),
            read("./game.txt"),
            edit("working", "working"),
            process(&["python3", "-V"]),
            read("/workspace/game.txt"),
            process(&["sh", "-c", "python3 --version"]),
            read("game.txt"),
            ScriptedTurn::text("must not finish"),
        ],
        dir.path(),
    )
    .await;
    let run = session
        .submit(
            "Repair",
            "start",
            Limits {
                max_repeated_calls: 1,
                ..Limits::continuous()
            },
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.error.unwrap().code, "repetition_limit");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "working"
    );
    let data = session.snapshot().await.unwrap();
    let receipts: Vec<_> = data
        .run(&run.id)
        .unwrap()
        .invocations
        .iter()
        .filter_map(|i| i.result.as_ref())
        .filter(|r| r.error_code.as_deref() == Some("repetition_recovery"))
        .collect();
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[1].data["recovery"], 2);
    engine.close().await.unwrap();
}

#[tokio::test]
async fn oversized_edits_get_smaller_contracts_until_real_progress_then_release() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "working").unwrap();
    let (engine, session, provider) = fixture(
        vec![
            read("game.txt"),
            edit("working", &"x".repeat(5000)),
            read("./game.txt"),
            edit("wrong anchor", "small replacement"),
            edit("working", &"y".repeat(3000)),
            edit("working", "repaired"),
            read("game.txt"),
        ],
        dir.path(),
    )
    .await;
    let run = session
        .submit(
            "Repair",
            "start",
            Limits {
                max_steps: 7,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
    let requests = provider.requests();
    let cap = |n: usize| {
        let schema = &requests[n]
            .tools
            .iter()
            .find(|t| t.name == "files.edit")
            .unwrap()
            .input_schema;
        schema["x-werk-repair-limits"]["game.txt"]
            .as_u64()
            .unwrap_or_else(|| schema["properties"]["new"]["maxLength"].as_u64().unwrap())
    };
    assert_eq!(cap(1), 4096);
    assert_eq!(cap(2), 2048);
    assert_eq!(cap(3), 2048);
    assert_eq!(cap(4), 2048);
    assert_eq!(cap(5), 1024);
    assert_eq!(cap(6), 4096);
    assert_eq!(
        task_state(&requests[5])["pending_repairs"][0]["attempts"],
        3
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "repaired"
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn repair_caps_are_path_scoped_and_report_actual_limits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "working").unwrap();
    let other = format!("unique{}", "y".repeat(3000));
    let (engine, session, provider) = fixture(
        vec![
            edit("working", &"x".repeat(5000)),
            call("files.write", json!({"path":"other.txt","content":other})),
            call("files.edit", json!({"path":"/workspace/./game.txt","expected_hash":hash("working"),"old":"working","new":"z".repeat(2500)})),
            call("files.edit", json!({"path":"./other.txt","expected_hash":hash(&other),"old":"unique","new":"n".repeat(1500)})),
            edit("working", "repaired"),
            read("game.txt"),
        ], dir.path(),
    ).await;
    let run = session
        .submit(
            "Repair",
            "start",
            Limits {
                max_steps: 6,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
    let data = session.snapshot().await.unwrap();
    let calls = &data.run(&run.id).unwrap().invocations;
    assert_eq!(
        calls[1].result.as_ref().unwrap().status,
        Outcome::Succeeded,
        "repair of game.txt must not cap creating other.txt"
    );
    let error = calls[2].result.as_ref().unwrap();
    assert_eq!(error.data["allowed_characters"], 2048);
    assert_eq!(error.data["max_new_characters"], 1024);
    assert!(
        error.data["message"]
            .as_str()
            .unwrap()
            .contains("2500 characters")
    );
    assert_eq!(
        calls[3].result.as_ref().unwrap().status,
        Outcome::Succeeded,
        "other.txt must remain editable above the unrelated 1024 size cap"
    );
    let requests = provider.requests();
    let schema = &requests[4]
        .tools
        .iter()
        .find(|t| t.name == "files.edit")
        .unwrap()
        .input_schema;
    assert_eq!(schema["properties"]["new"]["maxLength"], 4096);
    assert_eq!(schema["x-werk-repair-limits"]["game.txt"], 1024);
    assert!(schema["x-werk-repair-limits"]["other.txt"].is_null());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("other.txt")).unwrap(),
        format!("{}{}", "n".repeat(1500), "y".repeat(3000))
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "repaired"
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn invalid_anchor_size_does_not_shrink_new_content_budget() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "working").unwrap();
    let (engine, session, provider) = fixture(
        vec![
            edit(&"a".repeat(2100), "small"),
            edit("working", &"r".repeat(3000)),
            read("game.txt"),
        ],
        dir.path(),
    )
    .await;
    let run = session
        .submit(
            "Repair",
            "start",
            Limits {
                max_steps: 3,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
    let data = session.snapshot().await.unwrap();
    let first = data.run(&run.id).unwrap().invocations[0]
        .result
        .as_ref()
        .unwrap();
    assert_eq!(first.error_code.as_deref(), Some("coding_step_invalid"));
    assert!(first.data.get("max_new_characters").is_none());
    assert_eq!(
        task_state(&provider.requests()[1])["pending_repairs"][0]["size_limited"],
        false
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt"))
            .unwrap()
            .len(),
        3000
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn changing_bad_arguments_does_not_allow_unlimited_failed_repairs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "working").unwrap();
    let turns = (0..10)
        .map(|n| edit(&format!("invented-{n}"), "replacement"))
        .collect();
    let (engine, session, _) = fixture(turns, dir.path()).await;
    let run = session
        .submit("Repair", "start", Limits::continuous(), None)
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    let error = result.error.unwrap();
    assert_eq!(error.code, "repair_no_progress");
    assert!(error.message.contains("game.txt"));
    assert!(error.message.contains("write_conflict"));
    assert!(error.message.contains("unique nonempty match"));
    assert_eq!(result.metrics.tool_invocations, 5);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "working"
    );
    engine.close().await.unwrap();
}

#[tokio::test]
#[ignore = "explicit read-only replay of the user's saved session, no live server"]
async fn saved_session_repair_state_replay() {
    let database = std::env::var("WW_REPLAY_DATABASE").unwrap();
    let sid = std::env::var("WW_REPLAY_SESSION").unwrap();
    let connection =
        rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let raw: String = connection
        .query_row("SELECT data FROM sessions WHERE id=?1", [&sid], |r| {
            r.get(0)
        })
        .unwrap();
    let mut data: SessionData = serde_json::from_str(&raw).unwrap();
    let run = data.runs.last_mut().unwrap();
    // Replay the last failed read before the file was recreated. This checks
    // migration of old receipts, without requiring new process observations.
    let index = run
        .invocations
        .iter()
        .rposition(|i| {
            i.call.name == "files.read"
                && i.result
                    .as_ref()
                    .is_some_and(|r| r.error_code.as_deref() == Some("not_found"))
        })
        .unwrap();
    run.invocations.truncate(index + 1);
    let messages = run.invocations.last().unwrap().message_index + 2;
    data.messages.truncate(messages);
    run.contexts.clear();
    run.limits.context_tokens = 32768;
    run.limits.reserve_output_tokens = 4096;
    let id = run.id.clone();
    let (request, snapshot) = DefaultContextBuilder
        .build(&data, data.run(&id).unwrap(), vec![])
        .await
        .unwrap();
    let state = task_state(&request);
    let file = state["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "tetris.html")
        .unwrap();
    assert_eq!(file["status"], "missing");
    assert!(file.get("sha256").is_none());
    assert!(file.get("observed_tail_excerpt").is_none());
    assert!(!state["pending_repairs"].as_array().unwrap().is_empty());
    assert!(!snapshot.omitted_messages.is_empty());
    eprintln!(
        "Saved-session replay: {} canonical messages, {} omitted; tetris.html missing, no stale hash/tail, {} pending repair(s).",
        data.messages.len(),
        snapshot.omitted_messages.len(),
        state["pending_repairs"].as_array().unwrap().len()
    );
}

#[tokio::test]
async fn consolidation_verifies_remaining_files_without_recreating_deleted_sources() {
    if !sandbox().await {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (engine, session, _) = fixture(vec![
        call("files.write", json!({"path":"old.txt","content":"working"})),
        call("files.write", json!({"path":"game.txt","content":"consolidated"})),
        process(&["rm", "old.txt"]),
        call("process.run", json!({"argv":["sh","-c","test ! -e old.txt && test \"$(cat game.txt)\" = consolidated"],
            "verification":{"criteria":["Consolidated output works and obsolete source is gone"],"paths":["game.txt"]}})),
        ScriptedTurn::text("Verified consolidated output"),
    ], dir.path()).await;
    let run = session
        .submit(
            "Consolidate into game.txt",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert!(!dir.path().join("old.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "consolidated"
    );
    engine.close().await.unwrap();
}
