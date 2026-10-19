mod support;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use support::*;
use tempfile::TempDir;
use werk_worker::{policy::*, profiles::AgentProfile, providers::*, sessions::*, tools::*, *};

fn profile() -> AgentProfile {
    AgentProfile {
        schema_version: 1,
        name: "automation".into(),
        prompt: "Draft tickets".into(),
        tools: vec!["tickets.create".into()],
        skills: vec![],
    }
}
fn ticket() -> ScriptedTurn {
    ScriptedTurn::calls(
        "",
        vec![ToolCall {
            id: "ticket_call".into(),
            name: "tickets.create".into(),
            arguments: json!({"title":"Retry UI","description":"Explain failure","acceptance_criteria":["Shows retry"],"source":"notes.md:3"}),
        }],
    )
}
fn policy(origin: &str, preapproved: bool) -> HostPolicy {
    HostPolicy {
        grants: vec![CapabilityGrant {
            tool: "tickets.create".into(),
            version: "1".into(),
            resource: origin.into(),
            arguments_hash: None,
            expires_at_ms: None,
            preapproved,
        }],
        ..Default::default()
    }
}
fn registry(origin: &str) -> ToolRegistry {
    let mut r = ToolRegistry::builtins().unwrap();
    r.register(Arc::new(
        TicketConnector::new(
            &format!("{origin}/tickets"),
            Some("broker-secret".into()),
            true,
        )
        .unwrap(),
    ))
    .unwrap();
    r
}
async fn approval(run: &RunHandle) -> ApprovalRequest {
    let mut events = run.events(0);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(e) = events.next().await {
            if let EventKind::ApprovalRequested { request } = e.unwrap().event {
                return request;
            }
        }
        panic!("missing approval")
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn connector_posts_once_only_after_addressed_approval_and_denial_never_posts() {
    for allow in [false, true] {
        let server = Server::start(|r| {
            assert_eq!(r.method, "POST");
            assert_eq!(r.path, "/tickets");
            assert!(
                r.raw
                    .to_ascii_lowercase()
                    .contains("authorization: bearer broker-secret")
            );
            assert!(r.body.get("origin").is_none());
            Response::json(json!({"receipt":"ticket-1"}))
        })
        .await;
        let dir = TempDir::new().unwrap();
        let store = Arc::new(
            SqliteStore::open(dir.path().join("sessions.sqlite"))
                .await
                .unwrap(),
        );
        let workspace = TempDir::new().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            ticket(),
            ScriptedTurn::text("draft complete"),
        ]));
        let engine = WorkerEngine::builder(provider)
            .tools(registry(&server.origin))
            .store(store)
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                profile(),
                workspace.path(),
                policy(&server.origin, false),
                Value::Null,
            )
            .await
            .unwrap();
        let run = session
            .submit("create", "command", Limits::default(), None)
            .await
            .unwrap();
        let request = approval(&run).await;
        assert_eq!(server.requests.lock().unwrap().len(), 0);
        let decision = if allow {
            ApprovalDecision::AllowOnce
        } else {
            ApprovalDecision::Deny
        };
        run.respond_to_approval(&request.request_id, decision.clone(), "approval")
            .await
            .unwrap();
        assert!(matches!(
            run.respond_to_approval(&request.request_id, decision, "approval")
                .await
                .unwrap()
                .status,
            ReceiptStatus::Duplicate
        ));
        assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
        assert_eq!(server.requests.lock().unwrap().len(), usize::from(allow));
        assert!(
            !session
                .export()
                .await
                .unwrap()
                .to_string()
                .contains("broker-secret")
        );
        engine.close().await.unwrap();
    }
}
struct CrashStore {
    inner: Arc<SqliteStore>,
    crashed: AtomicBool,
    terminal: bool,
}
#[async_trait]
impl SessionStore for CrashStore {
    async fn load(&self, id: &str) -> Result<SessionData> {
        self.inner.load(id).await
    }
    async fn list(&self) -> Result<Vec<String>> {
        self.inner.list().await
    }
    async fn save(&self, data: &SessionData, expected: Option<u64>) -> Result<()> {
        let barrier = if self.terminal {
            data.runs.iter().any(|r| r.status == RunStatus::Completed)
        } else {
            data.runs
                .iter()
                .any(|r| r.invocations.iter().any(|i| i.result.is_some()))
        };
        if barrier || self.crashed.load(Ordering::Acquire) {
            self.crashed.store(true, Ordering::Release);
            return Err(WorkerError::new(
                "simulated_power_loss",
                "Test store cannot commit after selected crash barrier",
            ));
        }
        self.inner.save(data, expected).await
    }
    async fn close(&self) -> Result<()> {
        self.inner.close().await
    }
}
#[tokio::test]
async fn crash_after_external_effect_before_receipt_blocks_second_post() {
    let server = Server::start(|_| Response::json(json!({"receipt":"ticket-1"}))).await;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db.sqlite");
    let workspace = TempDir::new().unwrap();
    let store = Arc::new(CrashStore {
        inner: Arc::new(SqliteStore::open(&path).await.unwrap()),
        crashed: AtomicBool::new(false),
        terminal: false,
    });
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ticket()])))
        .store(store)
        .tools(registry(&server.origin))
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(),
            workspace.path(),
            policy(&server.origin, true),
            Value::Null,
        )
        .await
        .unwrap();
    let sid = session.id().await;
    let run = session
        .submit("create", "command", Limits::default(), None)
        .await
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run.wait())
        .await
        .unwrap();
    assert!(result.is_err());
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    engine.close().await.unwrap();
    let provider = Arc::new(ScriptedProvider::default());
    let engine = WorkerEngine::builder(provider.clone())
        .store(Arc::new(SqliteStore::open(&path).await.unwrap()))
        .tools(registry(&server.origin))
        .build()
        .await
        .unwrap();
    let session = engine.open_session(&sid).await.unwrap();
    let data = session.snapshot().await.unwrap();
    assert_eq!(
        data.run(&run.id).unwrap().invocations[0]
            .result
            .as_ref()
            .unwrap()
            .status,
        Outcome::OutcomeUnknown
    );
    assert_eq!(
        engine.resume_run(&sid, &run.id).await.err().unwrap().code,
        "outcome_unknown"
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert!(provider.requests().is_empty());
    engine.close().await.unwrap();
}
#[tokio::test]
async fn crash_after_receipt_and_final_response_replays_without_post_or_model() {
    let server = Server::start(|_| Response::json(json!({"receipt":"ticket-1"}))).await;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db.sqlite");
    let workspace = TempDir::new().unwrap();
    let store = Arc::new(CrashStore {
        inner: Arc::new(SqliteStore::open(&path).await.unwrap()),
        crashed: AtomicBool::new(false),
        terminal: true,
    });
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![
        ticket(),
        ScriptedTurn::text("saved final"),
    ])))
    .store(store)
    .tools(registry(&server.origin))
    .build()
    .await
    .unwrap();
    let session = engine
        .create_session(
            profile(),
            workspace.path(),
            policy(&server.origin, true),
            Value::Null,
        )
        .await
        .unwrap();
    let sid = session.id().await;
    let run = session
        .submit("create", "command", Limits::default(), None)
        .await
        .unwrap();
    assert!(run.wait().await.is_err());
    engine.close().await.unwrap();
    let provider = Arc::new(ScriptedProvider::default());
    let engine = WorkerEngine::builder(provider.clone())
        .store(Arc::new(SqliteStore::open(&path).await.unwrap()))
        .tools(registry(&server.origin))
        .build()
        .await
        .unwrap();
    let resumed = engine.resume_run(&sid, &run.id).await.unwrap();
    assert_eq!(
        resumed.wait().await.unwrap().text.as_deref(),
        Some("saved final")
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert!(provider.requests().is_empty());
    engine.close().await.unwrap();
}
