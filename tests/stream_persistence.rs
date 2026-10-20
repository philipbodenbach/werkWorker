use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{policy::*, profiles::AgentProfile, providers::*, sessions::SqliteStore, *};

fn profile() -> AgentProfile {
    AgentProfile {
        schema_version: 1,
        name: "stream-test".into(),
        prompt: "Write the requested file".into(),
        tools: vec!["files.write".into()],
        skills: vec![],
    }
}

#[tokio::test]
async fn token_fragments_are_lossless_without_one_sqlite_commit_per_token() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let body = "ü".repeat(3000);
    let arguments = json!({"path":"result.txt","content":body}).to_string();
    let mut deltas = Vec::new();
    for (n, ch) in arguments.chars().enumerate() {
        deltas.push(ModelDelta::Tool {
            index: 0,
            id: if n == 0 {
                "write".into()
            } else {
                String::new()
            },
            name: if n == 0 {
                "files.write".into()
            } else {
                String::new()
            },
            arguments: ch.to_string(),
        });
    }
    deltas.extend([
        ModelDelta::Finish {
            reason: "tool_calls".into(),
        },
        ModelDelta::Done,
    ]);
    let text = "Fertig ✓ ".repeat(300);
    let mut final_deltas: Vec<_> = text
        .chars()
        .map(|ch| ModelDelta::Text {
            text: ch.to_string(),
        })
        .collect();
    final_deltas.extend([
        ModelDelta::Finish {
            reason: "stop".into(),
        },
        ModelDelta::Done,
    ]);
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn {
            deltas,
            delay_ms: 0,
            error: None,
        },
        ScriptedTurn {
            deltas: final_deltas,
            delay_ms: 0,
            error: None,
        },
    ]));
    let store = root.path().join("session.sqlite");
    let engine = WorkerEngine::builder(provider)
        .store(Arc::new(SqliteStore::open(&store).await.unwrap()))
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(),
            &workspace,
            HostPolicy {
                workspace_writable: true,
                grants: vec![CapabilityGrant {
                    tool: "files.write".into(),
                    version: "1".into(),
                    resource: ".".into(),
                    arguments_hash: None,
                    expires_at_ms: None,
                    preapproved: true,
                }],
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
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.text.as_deref(), Some(text.as_str()));
    assert_eq!(
        std::fs::read_to_string(workspace.join("result.txt")).unwrap(),
        body
    );
    let data = session.snapshot().await.unwrap();
    let mut streamed_arguments = String::new();
    let mut streamed_text = String::new();
    for event in &data.events {
        match &event.event {
            EventKind::ToolDelta { arguments, .. } => streamed_arguments.push_str(arguments),
            EventKind::TextDelta { text, .. } => streamed_text.push_str(text),
            _ => {}
        }
    }
    assert_eq!(streamed_arguments, arguments);
    assert_eq!(streamed_text, text);
    assert_eq!(data.runs[0].attempts[1].partial_text, text);
    assert!(
        data.revision < 100,
        "thousands of fragments must not cause thousands of full session commits: {}",
        data.revision
    );
    let session_id = data.id.clone();
    engine.close().await.unwrap();
    use werk_worker::sessions::SessionStore;
    let reopened = SqliteStore::open(&store).await.unwrap();
    let persisted = reopened.load(&session_id).await.unwrap();
    assert_eq!(persisted.events.len(), data.events.len());
    assert_eq!(persisted.runs[0].final_text.as_deref(), Some(text.as_str()));
}

#[tokio::test]
async fn pending_text_is_flushed_on_provider_failure() {
    let root = tempfile::tempdir().unwrap();
    let text = "partial output ".repeat(50);
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedTurn {
        deltas: text
            .chars()
            .map(|ch| ModelDelta::Text {
                text: ch.to_string(),
            })
            .collect(),
        delay_ms: 0,
        error: Some(WorkerError::new("provider_error", "test failure")),
    }]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let session = engine
        .create_session(profile(), root.path(), HostPolicy::default(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("write", "start", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Failed);
    let data = session.snapshot().await.unwrap();
    assert_eq!(data.runs[0].attempts[0].partial_text, text);
    assert!(data.runs[0].invocations.is_empty());
    engine.close().await.unwrap();
}
