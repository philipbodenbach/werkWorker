use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use werk_worker::{
    agents::DelegateTask,
    policy::{HostPolicy, ToolPermission},
    profiles::AgentProfile,
    providers::*,
    sessions::SqliteStore,
    *,
};

struct Probe {
    root_count: AtomicUsize,
    child_count: AtomicUsize,
    root_started: tokio::sync::Notify,
    child_started: tokio::sync::Notify,
    root_release: Arc<tokio::sync::Notify>,
    child_release: Arc<tokio::sync::Notify>,
    requests: Mutex<Vec<(bool, ModelRequest)>>,
    counts: Mutex<std::collections::BTreeSet<String>>,
}
impl Probe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            root_count: AtomicUsize::new(0),
            child_count: AtomicUsize::new(0),
            root_started: tokio::sync::Notify::new(),
            child_started: tokio::sync::Notify::new(),
            root_release: Arc::new(tokio::sync::Notify::new()),
            child_release: Arc::new(tokio::sync::Notify::new()),
            requests: Mutex::new(vec![]),
            counts: Mutex::new(Default::default()),
        })
    }
}
#[async_trait]
impl ModelProvider for Probe {
    fn identity(&self) -> String {
        "reasoning-control-fixture".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            context_tokens: Some(32768),
            ..Default::default()
        }
    }
    fn concurrency(&self) -> InferenceConcurrency {
        InferenceConcurrency {
            max_requests: Some(3),
            source: "fixture".into(),
            detail: "verified".into(),
        }
    }
    async fn reasoning_options(&self, output: usize) -> Result<Option<ReasoningOptions>> {
        Ok(Some(serde_json::from_value(
            json!({"model":"fixture","requested":"default","native_effort":null,"profile":{
                "schema_version":1,"model":"fixture","supported":true,"default_tier":"low","default_adjusted_to_limits":false,
                "limits":{"context_tokens":32768,"output_tokens":output,"reasoning_tokens":output.saturating_sub(512),"answer_reserve_tokens":512},
                "tiers":[{"id":"low","label":"Low","available":true,"reasoning_tokens":512,"minimum_output_tokens":1024},
                    {"id":"high","label":"High","available":output>=2560,"reasoning_tokens":2048,"minimum_output_tokens":2560},
                    {"id":"ultra","label":"Ultra","available":output>=8704,"reasoning_tokens":8192,"minimum_output_tokens":8704}]
            }}),
        )?))
    }
    async fn count_input_tokens(&self, request: &ModelRequest) -> Result<Option<usize>> {
        self.counts.lock().unwrap().insert(fingerprint(request));
        Ok(Some(64))
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        assert!(
            self.counts.lock().unwrap().contains(&fingerprint(&request)),
            "counted settings must match dispatched settings"
        );
        let child = request
            .messages
            .iter()
            .any(|m| m.role == Role::System && m.content.contains("delegated worker"));
        self.requests.lock().unwrap().push((child, request));
        let n = if child {
            self.child_count.fetch_add(1, Ordering::SeqCst)
        } else {
            self.root_count.fetch_add(1, Ordering::SeqCst)
        };
        let gate = if child {
            self.child_release.clone()
        } else {
            self.root_release.clone()
        };
        if n == 0 {
            if child {
                self.child_started.notify_one()
            } else {
                self.root_started.notify_one()
            }
        }
        let turn = if n == 0 {
            ScriptedTurn::calls(
                "",
                vec![ToolCall {
                    id: id("call"),
                    name: "files.list".into(),
                    arguments: json!({"path":"."}),
                }],
            )
        } else {
            ScriptedTurn::text("Read-only work complete")
        };
        Ok(Box::pin(
            async_stream::stream! {if n==0{gate.notified().await;}for delta in turn.deltas{yield Ok(delta);}},
        ))
    }
}

#[tokio::test]
async fn effort_changes_next_parent_and_existing_child_requests_and_survives_reopen() {
    let provider = Probe::new();
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("sessions.sqlite");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let engine = WorkerEngine::builder(provider.clone())
        .store(Arc::new(SqliteStore::open(&store).await.unwrap()))
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "fixture".into(),
                prompt: "Inspect".into(),
                tools: vec!["files.list".into(), "delegate_task".into()],
                skills: vec![],
            },
            &workspace,
            HostPolicy {
                tool_permissions: [("files.list".into(), ToolPermission::Allow)].into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let limits = Limits {
        reserve_output_tokens: 4096,
        ..Limits::continuous()
    };
    let original = session.reasoning_options(&limits).await.unwrap();
    assert_eq!(
        original.label(),
        "Server default: Low; 512 reasoning tokens"
    );
    let run = session
        .submit("Read files", "start", limits.clone(), None)
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.root_started.notified(),
    )
    .await
    .unwrap();
    run.delegate(
        vec![DelegateTask {
            goal: "Inspect independently".into(),
            context: String::new(),
            tools: Some(vec!["files.list".into()]),
            read_only: true,
        }],
        "child",
    )
    .await
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.child_started.notified(),
    )
    .await
    .unwrap();
    assert_eq!(
        session
            .set_reasoning_tier("high", &limits)
            .await
            .unwrap()
            .requested,
        "high"
    );
    assert_eq!(
        session
            .set_reasoning_tier("ultra", &limits)
            .await
            .unwrap_err()
            .code,
        "invalid_reasoning_tier"
    );
    assert_eq!(
        session.reasoning_options(&limits).await.unwrap().requested,
        "high"
    );
    provider.root_release.notify_one();
    provider.child_release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    {
        let requests = provider.requests.lock().unwrap();
        for child in [false, true] {
            let turns: Vec<_> = requests
                .iter()
                .filter(|(is_child, _)| *is_child == child)
                .map(|(_, r)| r)
                .collect();
            assert!(turns.len() >= 2);
            assert_eq!(turns[0].reasoning_tier, None);
            assert!(
                turns[1..]
                    .iter()
                    .all(|r| r.reasoning_tier.as_deref() == Some("high"))
            );
        }
    }
    let sid = session.id().await;
    engine.close().await.unwrap();
    let reopened = WorkerEngine::builder(provider.clone())
        .store(Arc::new(SqliteStore::open(&store).await.unwrap()))
        .build()
        .await
        .unwrap();
    let session = reopened.open_session(&sid).await.unwrap();
    assert_eq!(
        session.reasoning_options(&limits).await.unwrap().requested,
        "high"
    );
    assert_eq!(
        session
            .set_reasoning_tier("default", &limits)
            .await
            .unwrap()
            .label(),
        "Server default: Low; 512 reasoning tokens"
    );
    let next = session
        .submit("Continue inspection", "followup", limits, None)
        .await
        .unwrap();
    assert_eq!(next.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(
        provider
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .1
            .reasoning_tier
            .as_deref(),
        Some("default")
    );
    reopened.close().await.unwrap();
}
