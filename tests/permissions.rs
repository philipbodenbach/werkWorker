use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use werk_worker::{
    policy::{ApprovalMode, HostPolicy, PolicyDecision},
    profiles::AgentProfile,
    providers::{ScriptedProvider, ScriptedTurn},
    runtime::Sandbox,
    sessions::SqliteStore,
    tools::ToolRegistry,
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

async fn approval(run: &RunHandle, after: u64) -> ApprovalRequest {
    let mut events = run.events(after);
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.next().await {
            if let EventKind::ApprovalRequested { request } = event.unwrap().event {
                return request;
            }
        }
        panic!("No approval request");
    })
    .await
    .unwrap()
}

async fn answer(run: &RunHandle, request: &ApprovalRequest, allow: bool) {
    run.respond_to_approval(
        &request.request_id,
        if allow {
            ApprovalDecision::AllowOnce
        } else {
            ApprovalDecision::Deny
        },
        &id("answer"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn host_upgrade_during_question_enables_real_writes_and_persists_in_same_session() {
    let workspace = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let path = storage.path().join("sessions.sqlite");
    let provider = Arc::new(ScriptedProvider::new(vec![
        call(
            "interaction.ask",
            json!({"question":"May I create the files?","choices":["yes","no"]}),
        ),
        call("skills.load", json!({"name":"coding-basic"})),
        call(
            "files.write",
            json!({"path":"denied.txt","content":"must not exist"}),
        ),
        call(
            "files.write",
            json!({"path":"approved.txt","content":"actually written"}),
        ),
        ScriptedTurn::text("The file was written."),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .store(Arc::new(SqliteStore::open(&path).await.unwrap()))
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            AgentProfile::bundled("general-readonly").unwrap(),
            workspace.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let session_id = session.id().await;
    let run = session
        .submit(
            "Create files",
            "start",
            Limits {
                context_tokens: 16384,
                reserve_output_tokens: 4096,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let question = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = run.status().await.unwrap();
            if let Some(q) = status
                .invocations
                .iter()
                .find_map(|i| i.interaction.clone())
            {
                break q;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!session.snapshot().await.unwrap().policy.workspace_writable);
    session.enable_tool_approvals("enable").await.unwrap();
    assert!(matches!(
        session
            .enable_tool_approvals("enable")
            .await
            .unwrap()
            .status,
        ReceiptStatus::Duplicate
    ));
    run.respond_to_interaction(&question.request_id, "ja", "question-answer")
        .await
        .unwrap();
    let denied = approval(&run, 0).await;
    assert_eq!(denied.description["tool"], "files.write");
    assert!(!workspace.path().join("denied.txt").exists());
    let cursor = session
        .snapshot()
        .await
        .unwrap()
        .events
        .last()
        .unwrap()
        .cursor;
    answer(&run, &denied, false).await;
    let allowed = approval(&run, cursor).await;
    assert_ne!(allowed.request_id, denied.request_id);
    assert!(!workspace.path().join("approved.txt").exists());
    assert!(
        run.respond_to_approval(&denied.request_id, ApprovalDecision::AllowOnce, "stale")
            .await
            .is_err()
    );
    answer(&run, &allowed, true).await;
    let result = tokio::time::timeout(Duration::from_secs(5), run.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("approved.txt")).unwrap(),
        "actually written"
    );
    assert!(!workspace.path().join("denied.txt").exists());
    assert_eq!(session.id().await, session_id);
    let state = session.snapshot().await.unwrap();
    assert!(state.policy.grants.is_empty());
    assert!(state.skill_snapshots.contains_key("coding-basic"));
    let loaded = state.runs[0]
        .invocations
        .iter()
        .find(|i| i.call.name == "skills.load")
        .unwrap();
    assert_eq!(loaded.result.as_ref().unwrap().status, Outcome::Succeeded);
    assert_eq!(state.profile.tools.len(), engine.tools().len());
    assert!(
        provider.requests()[1]
            .tools
            .iter()
            .any(|t| t.name == "process.run")
    );
    engine.close().await.unwrap();

    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![
        call(
            "files.write",
            json!({"path":"after-restart.txt","content":"fresh approval"}),
        ),
        ScriptedTurn::text("done"),
    ])))
    .store(Arc::new(SqliteStore::open(&path).await.unwrap()))
    .build()
    .await
    .unwrap();
    let session = engine.open_session(&session_id).await.unwrap();
    assert!(session.snapshot().await.unwrap().policy.request_approvals);
    let run = session
        .submit("Continue", "next", Limits::default(), None)
        .await
        .unwrap();
    let request = approval(&run, 0).await;
    assert!(!workspace.path().join("after-restart.txt").exists());
    answer(&run, &request, true).await;
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert!(workspace.path().join("after-restart.txt").exists());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn plain_question_answer_cannot_grant_permissions() {
    let workspace = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        call(
            "interaction.ask",
            json!({"question":"Can I write?","choices":["yes"]}),
        ),
        call(
            "files.write",
            json!({"path":"not-authorized","content":"no"}),
        ),
        ScriptedTurn::text("blocked"),
    ]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let session = engine
        .create_session(
            AgentProfile::bundled("coding-basic").unwrap(),
            workspace.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("go", "start", Limits::default(), None)
        .await
        .unwrap();
    let mut events = run.events(0);
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.next().await {
            if let EventKind::InteractionRequested { request } = event.unwrap().event {
                run.respond_to_interaction(&request.request_id, "ja", "yes")
                    .await
                    .unwrap();
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert!(!workspace.path().join("not-authorized").exists());
    assert!(!session.snapshot().await.unwrap().policy.request_approvals);
    assert_eq!(
        run.status().await.unwrap().invocations[1]
            .result
            .as_ref()
            .unwrap()
            .status,
        Outcome::Denied
    );
    engine.close().await.unwrap();
}

#[test]
fn request_mode_never_bypasses_hard_denies_paths_or_read_only_workers() {
    let registry = ToolRegistry::builtins().unwrap();
    let write = registry.get("files.write").unwrap().definition();
    let read = registry.get("files.read").unwrap().definition();
    let mut policy = HostPolicy {
        request_approvals: true,
        workspace_writable: true,
        mode: ApprovalMode::FullAccess,
        ..Default::default()
    };
    assert_eq!(
        policy.evaluate(&write, &json!({"path":"a"}), &[]),
        PolicyDecision::Ask
    );
    assert_eq!(
        policy.evaluate(&write, &json!({"path":"../outside"}), &[]),
        PolicyDecision::Deny
    );
    policy.hard_denied_tools.push("files.write".into());
    assert_eq!(
        policy.evaluate(&write, &json!({"path":"a"}), &[]),
        PolicyDecision::Deny
    );
    policy.hard_denied_tools.clear();
    policy.workspace_writable = false;
    assert_eq!(
        policy.evaluate(&write, &json!({"path":"a"}), &[]),
        PolicyDecision::Deny
    );
    assert_eq!(
        policy.evaluate(&read, &json!({"path":"a"}), &[]),
        PolicyDecision::Ask
    );
}

#[tokio::test]
async fn explicit_process_approval_runs_in_the_real_workspace_sandbox() {
    let sandbox = Sandbox::default();
    if let Err(error) = sandbox.doctor().await {
        if std::env::var_os("REQUIRE_SANDBOX").is_some() {
            panic!("Required real sandbox unavailable: {error}");
        }
        eprintln!("SKIP real sandbox: {error}");
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        call(
            "process.run",
            json!({"argv":["/bin/sh","-c","printf verified > process-proof; printf process-ok"],"cwd":"."}),
        ),
        ScriptedTurn::text("done"),
    ]));
    let engine = WorkerEngine::builder(provider)
        .sandbox(sandbox)
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            AgentProfile::bundled("general-readonly").unwrap(),
            workspace.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    session.enable_tool_approvals("enable").await.unwrap();
    let run = session
        .submit("Run verification", "start", Limits::default(), None)
        .await
        .unwrap();
    let request = approval(&run, 0).await;
    assert_eq!(
        request.description["arguments"]["sandbox"]["network"],
        false
    );
    assert_eq!(
        request.description["arguments"]["sandbox"]["workspace_writable"],
        true
    );
    assert!(!workspace.path().join("process-proof").exists());
    answer(&run, &request, true).await;
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("process-proof")).unwrap(),
        "verified"
    );
    assert_eq!(
        run.status().await.unwrap().invocations[0]
            .result
            .as_ref()
            .unwrap()
            .status,
        Outcome::Succeeded
    );
    engine.close().await.unwrap();
}
