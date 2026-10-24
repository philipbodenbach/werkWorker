use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;
use werk_worker::{
    context::{estimate_request, validate_messages},
    harness::HarnessMode,
    policy::{HostPolicy, ToolPermission},
    profiles::AgentProfile,
    providers::*,
    *,
};

const UPDATE: &str = "and put it all in one file";
#[derive(Clone, Copy, PartialEq)]
enum Hold {
    Planning,
    Retry,
    Streaming,
}
struct Probe {
    hold: Hold,
    blocked: AtomicBool,
    calls: AtomicUsize,
    started: Notify,
    release: Arc<Notify>,
    requests: Mutex<Vec<ModelRequest>>,
}
impl Probe {
    fn active(request: &ModelRequest) -> bool {
        request
            .messages
            .iter()
            .any(|m| m.role == Role::User && m.content == "Build the game")
    }
}
#[async_trait]
impl ModelProvider for Probe {
    fn identity(&self) -> String {
        "steering-fixture".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            context_tokens: Some(8192),
            ..Default::default()
        }
    }
    async fn count_input_tokens(&self, request: &ModelRequest) -> Result<Option<usize>> {
        if self.hold == Hold::Planning
            && Self::active(request)
            && !self.blocked.swap(true, Ordering::SeqCst)
        {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(Some(estimate_request(request)))
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        validate_messages(&request.messages)?;
        if !Self::active(&request) {
            return ScriptedProvider::new(vec![ScriptedTurn::text(
                &"old implementation ".repeat(1600),
            )])
            .stream(request)
            .await;
        }
        self.requests.lock().unwrap().push(request.clone());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 && self.hold == Hold::Retry {
            self.started.notify_one();
            self.release.notified().await;
            let mut error = WorkerError::new("temporary_fixture_error", "Retry fixture");
            error.retryable = true;
            error.retry_after_ms = Some(1);
            return Err(error);
        }
        let wait = n == 0 && self.hold == Hold::Streaming;
        let gate = self.release.clone();
        let turn = if wait {
            self.started.notify_one();
            ScriptedTurn::calls(
                "Finishing the current observation",
                vec![ToolCall {
                    id: id("call"),
                    name: "files.list".into(),
                    arguments: json!({"path":"."}),
                }],
            )
        } else {
            ScriptedTurn::text("Updated task verified")
        };
        Ok(Box::pin(async_stream::stream! {
            if wait { gate.notified().await; }
            for delta in turn.deltas { yield Ok(delta); }
        }))
    }
}

#[tokio::test]
async fn steering_survives_compaction_and_replans_before_dispatch_or_retry() {
    for mode in [
        HarnessMode::Classic,
        HarnessMode::Minimal,
        HarnessMode::Adaptive,
    ] {
        for hold in [Hold::Planning, Hold::Retry, Hold::Streaming] {
            let provider = Arc::new(Probe {
                hold,
                blocked: AtomicBool::new(false),
                calls: AtomicUsize::new(0),
                started: Notify::new(),
                release: Arc::new(Notify::new()),
                requests: Mutex::new(vec![]),
            });
            let dir = tempfile::tempdir().unwrap();
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
                    AgentProfile {
                        schema_version: 1,
                        name: "fixture".into(),
                        prompt: "Implement the user's task".into(),
                        tools: vec!["files.list".into()],
                        skills: vec![],
                    },
                    dir.path(),
                    HostPolicy {
                        tool_permissions: [("files.list".into(), ToolPermission::Allow)].into(),
                        ..Default::default()
                    },
                    Value::Null,
                )
                .await
                .unwrap();
            let limits = Limits {
                context_tokens: 8192,
                ..Default::default()
            };
            let seed = session
                .submit("Earlier task", "seed", limits.clone(), None)
                .await
                .unwrap();
            assert_eq!(seed.wait().await.unwrap().status, RunStatus::Completed);
            let run = session
                .submit("Build the game", "start", limits, None)
                .await
                .unwrap();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                provider.started.notified(),
            )
            .await
            .unwrap();
            run.steer(UPDATE, "update").await.unwrap();
            run.steer(UPDATE, "update").await.unwrap(); // retries of the host command are idempotent
            provider.release.notify_one();
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), run.wait())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
            let requests = provider.requests.lock().unwrap().clone();
            assert_eq!(requests.len(), if hold == Hold::Planning { 1 } else { 2 });
            let last = requests.last().unwrap();
            assert_eq!(
                last.messages
                    .iter()
                    .filter(|m| m.role == Role::User && m.content == UPDATE)
                    .count(),
                1
            );
            assert!(
                last.messages
                    .iter()
                    .any(|m| m.role == Role::User && m.content == "Build the game")
            );
            assert!(last.messages[0].content.contains("update the active task"));
            let data = session.snapshot().await.unwrap();
            let steering_index = data
                .messages
                .iter()
                .position(|m| m.role == Role::User && m.content == UPDATE)
                .unwrap();
            let state = run.status().await.unwrap();
            assert!(!state.steering_context_pending);
            assert!(!state.contexts.last().unwrap().omitted_messages.is_empty());
            assert!(
                !state
                    .contexts
                    .last()
                    .unwrap()
                    .omitted_messages
                    .contains(&steering_index)
            );
            for code in ["steering_queued", "steering_applied"] {
                assert_eq!(
                    data.events
                        .iter()
                        .filter(|e| matches!(&e.event, EventKind::Diagnostic{code:c} if c==code))
                        .count(),
                    1
                );
            }
            engine.close().await.unwrap();
        }
    }
}
