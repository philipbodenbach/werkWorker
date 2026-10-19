use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{policy::*, profiles::AgentProfile, providers::*, runtime::ArtifactStore, *};

fn profile(tools: &[&str]) -> AgentProfile {
    AgentProfile {
        schema_version: 1,
        name: "fixture".into(),
        prompt: "Keep hard constraints".into(),
        tools: tools.iter().map(|s| s.to_string()).collect(),
        skills: vec![],
    }
}
fn read(path: &str) -> ScriptedTurn {
    ScriptedTurn::calls(
        "",
        vec![ToolCall {
            id: id("call"),
            name: "files.read".into(),
            arguments: json!({"path":path}),
        }],
    )
}
fn policy(similar: bool) -> HostPolicy {
    HostPolicy {
        allow_similar: similar,
        grants: vec![CapabilityGrant {
            tool: "files.read".into(),
            version: "1".into(),
            resource: ".".into(),
            arguments_hash: None,
            expires_at_ms: None,
            preapproved: false,
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn model_compaction_is_budgeted_preserves_user_constraints_and_raw_history() {
    let dir = tempfile::tempdir().unwrap();
    let long = "Historical evidence. ".repeat(100);
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::text(&long),
        ScriptedTurn::text("Earlier evidence summary."),
        ScriptedTurn::text("Final answer."),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(profile(&[]), dir.path(), HostPolicy::default(), Value::Null)
        .await
        .unwrap();
    let first = session
        .submit(
            "Never change customer identifiers",
            "first",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    first.wait().await.unwrap();
    let limits = Limits {
        model_compaction: true,
        ..Default::default()
    };
    let second = session
        .submit("Summarize next", "second", limits, None)
        .await
        .unwrap();
    assert_eq!(second.wait().await.unwrap().status, RunStatus::Completed);
    let state = session.snapshot().await.unwrap();
    assert_eq!(state.compactions.len(), 1);
    assert_eq!(state.compactions[0].message_indices, vec![1]);
    assert!(state.messages.iter().any(|m| m.content == long));
    assert_eq!(second.status().await.unwrap().metrics.model_requests, 2);
    let requests = provider.requests();
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|m| m.content == "Never change customer identifiers")
    );
    assert!(!requests[2].messages.iter().any(|m| m.content == long));
    assert!(
        requests[2].messages[0]
            .content
            .contains("Earlier evidence summary")
    );
    engine.close().await.unwrap();
}
#[tokio::test]
async fn same_scope_approval_is_exact_explicit_and_does_not_cross_session() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), "alpha").unwrap();
    std::fs::write(dir.path().join("b"), "beta").unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        read("a"),
        read("a"),
        read("b"),
        ScriptedTurn::text("done"),
    ]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let session = engine
        .create_session(
            profile(&["files.read"]),
            dir.path(),
            policy(true),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("read", "c", Limits::default(), None)
        .await
        .unwrap();
    let mut events = run.events(0);
    let mut approvals = vec![];
    while let Some(e) = events.next().await {
        if let EventKind::ApprovalRequested { request } = e.unwrap().event {
            approvals.push(request.description["arguments"]["path"].clone());
            run.respond_to_approval(
                &request.request_id,
                ApprovalDecision::AllowSameArguments,
                &id("approve"),
            )
            .await
            .unwrap();
        }
    }
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(approvals, vec![json!("a"), json!("b")]);
    let another = engine
        .create_session(
            profile(&["files.read"]),
            dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    assert!(another.snapshot().await.unwrap().policy.grants.is_empty());
    engine.close().await.unwrap();
}
#[tokio::test]
async fn revocation_and_cross_run_approval_cannot_start_action() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), "alpha").unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![read("a")]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let session = engine
        .create_session(
            profile(&["files.read"]),
            dir.path(),
            policy(false),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("read", "c", Limits::default(), None)
        .await
        .unwrap();
    let mut events = run.events(0);
    let request = loop {
        if let EventKind::ApprovalRequested { request } =
            events.next().await.unwrap().unwrap().event
        {
            break request;
        }
    };
    session.revoke_tool("files.read", "revoke").await.unwrap();
    run.respond_to_approval(&request.request_id, ApprovalDecision::AllowOnce, "allow")
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().code, "grant_revoked");
    assert!(
        !session
            .snapshot()
            .await
            .unwrap()
            .events
            .iter()
            .any(|e| matches!(e.event, EventKind::ToolStarted { .. }))
    );
    engine.close().await.unwrap();
}
#[tokio::test]
async fn separate_sessions_run_independently_shared_workspace_conflicts() {
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn {
            delay_ms: 150,
            ..ScriptedTurn::text("one")
        },
        ScriptedTurn {
            delay_ms: 150,
            ..ScriptedTurn::text("two")
        },
    ]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let first = engine
        .create_session(
            profile(&[]),
            first_dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let second = engine
        .create_session(
            profile(&[]),
            second_dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let shared = engine
        .create_session(
            profile(&[]),
            first_dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let run = first
        .submit("first", "c", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(
        shared
            .submit("collision", "x", Limits::default(), None)
            .await
            .err()
            .unwrap()
            .code,
        "workspace_busy"
    );
    let other = second
        .submit("second", "c", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(other.wait().await.unwrap().status, RunStatus::Completed);
    assert!(
        !second
            .snapshot()
            .await
            .unwrap()
            .messages
            .iter()
            .any(|m| m.content == "first")
    );
    engine.close().await.unwrap();
}
#[tokio::test]
async fn large_output_has_owned_artifact_and_context_excerpt() {
    let dir = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("large"), "a".repeat(20_000)).unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        read("large"),
        ScriptedTurn::text("done"),
    ]));
    let engine = WorkerEngine::builder(provider)
        .artifacts(ArtifactStore::open(artifacts.path()).await.unwrap())
        .build()
        .await
        .unwrap();
    let mut policy = policy(false);
    policy.grants[0].preapproved = true;
    let session = engine
        .create_session(profile(&["files.read"]), dir.path(), policy, Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("read", "c", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let state = run.status().await.unwrap();
    let result = state.invocations[0].result.as_ref().unwrap();
    assert!(result.truncated);
    assert_eq!(result.artifacts.len(), 1);
    assert!(result.data.to_string().len() < 9000);
    engine.close().await.unwrap();
}
