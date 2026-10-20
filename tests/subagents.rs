use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use werk_worker::{
    agents::DelegateTask,
    parallel::ExecutionConfig,
    policy::{CapabilityGrant, HostPolicy},
    profiles::AgentProfile,
    providers::*,
    *,
};

fn tool_turn(id: &str, name: &str, arguments: Value) -> ScriptedTurn {
    ScriptedTurn::calls(
        "",
        vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }],
    )
}

struct Probe {
    capacity: Option<usize>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    seen: Mutex<Vec<ModelRequest>>,
    child_delay: u64,
    write: bool,
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Probe {
    fn new(capacity: Option<usize>, delay: u64, write: bool) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            seen: Mutex::new(vec![]),
            child_delay: delay,
            write,
        })
    }
}
#[async_trait]
impl ModelProvider for Probe {
    fn identity(&self) -> String {
        "probe".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn concurrency(&self) -> InferenceConcurrency {
        InferenceConcurrency {
            max_requests: self.capacity,
            source: "test".into(),
            detail: "test".into(),
        }
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let child = request
            .messages
            .iter()
            .any(|m| m.role == Role::System && m.content.contains("delegated worker"));
        let has_tool = request.messages.iter().any(|m| m.role == Role::Tool);
        let collected = request
            .messages
            .iter()
            .any(|m| m.content.starts_with("Subagent results"));
        self.seen.lock().unwrap().push(request);
        let deltas = if child && self.write && !has_tool {
            tool_turn(
                "write",
                "files.write",
                json!({"path":"result.txt","content":"changed","expected_hash":hash("original")}),
            )
            .deltas
        } else if !child && !has_tool && !collected {
            tool_turn("delegate", "delegate_task", json!({"tasks":[{"goal":"alpha","context":"explicit child context"},{"goal":"beta"}]})).deltas
        } else {
            ScriptedTurn::text(if collected {
                "synthesized"
            } else if child {
                "child result"
            } else {
                "parent interim"
            })
            .deltas
        };
        let active = self.active.clone();
        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(count, Ordering::SeqCst);
        let guard = Active(active);
        let delay = if child { self.child_delay } else { 1 };
        Ok(Box::pin(async_stream::stream! {
            let _guard = guard;
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            for delta in deltas { yield Ok(delta); }
        }))
    }
}
fn profile(write: bool) -> AgentProfile {
    let mut tools = vec![
        "delegate_task".into(),
        "agents.wait".into(),
        "agents.status".into(),
    ];
    if write {
        tools.push("files.write".into());
    }
    AgentProfile {
        schema_version: 1,
        name: "orchestrator".into(),
        prompt: "Follow the assigned task.".into(),
        tools,
        skills: vec![],
    }
}
async fn setup(
    provider: Arc<Probe>,
    config: ExecutionConfig,
    write: bool,
) -> (WorkerEngine, SessionHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("result.txt"), "original").unwrap();
    let engine = WorkerEngine::builder(provider)
        .execution(config)
        .build()
        .await
        .unwrap();
    let policy = HostPolicy {
        workspace_writable: write,
        grants: if write {
            vec![CapabilityGrant {
                tool: "files.write".into(),
                version: "1".into(),
                resource: ".".into(),
                arguments_hash: None,
                expires_at_ms: None,
                preapproved: true,
            }]
        } else {
            vec![]
        },
        ..Default::default()
    };
    let session = engine
        .create_session(profile(write), dir.path(), policy, Value::Null)
        .await
        .unwrap();
    (engine, session, dir)
}
#[tokio::test]
async fn children_overlap_context_isolated_and_results_are_joined() {
    let provider = Probe::new(Some(4), 100, false);
    let (engine, session, _dir) = setup(provider.clone(), ExecutionConfig::default(), false).await;
    let run = session
        .submit("private parent history", "submit", Limits::default(), None)
        .await
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(result.text.as_deref(), Some("synthesized"));
    assert!(provider.peak.load(Ordering::SeqCst) >= 2);
    assert!(provider.peak.load(Ordering::SeqCst) <= 4);
    let children = run.subagents().await.unwrap();
    assert_eq!(children.len(), 2);
    for child in &children {
        let data = engine
            .open_session(&child.session_id)
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap();
        assert!(!data.profile.tools.iter().any(|t| t == "delegate_task"));
        assert!(
            !data
                .messages
                .iter()
                .any(|m| m.content.contains("private parent history"))
        );
        assert_eq!(data.parent.as_ref().unwrap().root_run, run.id);
    }
    let snapshot = run.status().await.unwrap();
    assert!(snapshot.children.iter().all(|c| c.delivered));
    assert_eq!(
        snapshot.tree_model_requests as usize,
        provider.seen.lock().unwrap().len()
    );
    assert_eq!(engine.execution_status()["free_subagent_slots"], 4);
    engine.close().await.unwrap();
}
#[tokio::test]
async fn unknown_capacity_serializes_inference_without_blocking_delegation() {
    let provider = Probe::new(None, 35, false);
    let (engine, session, _dir) = setup(provider.clone(), ExecutionConfig::default(), false).await;
    let run = session
        .submit("go", "submit", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(run.subagents().await.unwrap().len(), 2);
    assert_eq!(provider.peak.load(Ordering::SeqCst), 1);
    assert_eq!(engine.execution_status()["effective_model_requests"], 1);
    engine.close().await.unwrap();
}
#[tokio::test]
async fn cancellation_cleans_children_and_releases_capacity() {
    let provider = Probe::new(Some(2), 3000, false);
    let (engine, session, _dir) = setup(provider.clone(), ExecutionConfig::default(), false).await;
    let run = session
        .submit("go", "submit", Limits::default(), None)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if run.subagents().await.unwrap().len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    run.cancel("stop", "stop").await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), run.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, RunStatus::Cancelled);
    for child in run.subagents().await.unwrap() {
        assert!(child.run.unwrap().status.terminal());
    }
    assert_eq!(provider.active.load(Ordering::SeqCst), 0);
    assert_eq!(engine.execution_status()["free_model_slots"], 2);
    engine.close().await.unwrap();
}
#[tokio::test]
async fn total_tree_request_budget_is_enforced() {
    let provider = Probe::new(Some(4), 20, false);
    let (engine, session, _dir) = setup(provider.clone(), ExecutionConfig::default(), false).await;
    let run = session
        .submit(
            "go",
            "submit",
            Limits {
                max_model_requests: 3,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(run.status().await.unwrap().tree_model_requests, 3);
    assert_eq!(provider.seen.lock().unwrap().len(), 3);
    engine.close().await.unwrap();
}
#[tokio::test]
async fn child_scope_and_identity_are_not_model_controlled() {
    let provider = Probe::new(Some(4), 200, false);
    let (engine, session, _dir) = setup(provider, ExecutionConfig::default(), false).await;
    let run = session
        .submit("go", "submit", Limits::default(), None)
        .await
        .unwrap();
    let task = DelegateTask {
        goal: "escape".into(),
        context: String::new(),
        tools: Some(vec!["process.run".into()]),
        read_only: false,
    };
    assert_eq!(
        run.delegate(vec![task], "bad").await.unwrap_err().code,
        "agent_scope"
    );
    assert_eq!(
        run.subagent("foreign").await.err().unwrap().code,
        "agent_denied"
    );
    run.cancel("stop", "stop").await.unwrap();
    run.wait().await.unwrap();
    engine.close().await.unwrap();
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn concurrent_writers_detect_stale_hash_instead_of_overwriting() {
    let provider = Probe::new(Some(4), 20, true);
    let (engine, session, dir) = setup(provider, ExecutionConfig::default(), true).await;
    let run = session
        .submit("go", "submit", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let outcomes: Vec<_> = run
        .subagents()
        .await
        .unwrap()
        .into_iter()
        .flat_map(|c| c.run.unwrap().invocations)
        .filter(|i| i.call.name == "files.write")
        .map(|i| i.result.unwrap())
        .collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|r| r.status == Outcome::Succeeded)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|r| r.error_code.as_deref() == Some("write_conflict"))
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
        "changed"
    );
    engine.close().await.unwrap();
}

async fn wait_for_children(run: &RunHandle) -> Vec<werk_worker::agents::ChildStatus> {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let children = run.subagents().await.unwrap();
            if children.len() >= 2 {
                return children;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}
fn task(goal: &str) -> DelegateTask {
    DelegateTask {
        goal: goal.into(),
        context: String::new(),
        tools: None,
        read_only: true,
    }
}
#[tokio::test]
async fn delegation_is_idempotent_and_global_capacity_is_bounded() {
    let (engine, session, _dir) = setup(
        Probe::new(Some(4), 1000, false),
        ExecutionConfig::default(),
        false,
    )
    .await;
    let run = session
        .submit("go", "submit", Limits::default(), None)
        .await
        .unwrap();
    wait_for_children(&run).await;
    run.delegate(vec![task("extra")], "extra").await.unwrap();
    run.delegate(vec![task("extra")], "extra").await.unwrap();
    assert_eq!(run.subagents().await.unwrap().len(), 3);
    assert_eq!(
        run.delegate(vec![task("changed")], "extra")
            .await
            .unwrap_err()
            .code,
        "command_conflict"
    );
    assert_eq!(
        run.delegate(vec![task("four"), task("five")], "overflow")
            .await
            .unwrap_err()
            .code,
        "agent_capacity"
    );
    assert_eq!(run.subagents().await.unwrap().len(), 3);
    run.cancel("stop", "stop").await.unwrap();
    run.wait().await.unwrap();
    assert_eq!(engine.execution_status()["free_subagent_slots"], 4);
    engine.close().await.unwrap();
}
#[tokio::test]
async fn nested_children_obey_depth_and_cancel_with_root() {
    let (engine, session, _dir) = setup(
        Probe::new(Some(4), 1000, false),
        ExecutionConfig {
            max_depth: 2,
            ..Default::default()
        },
        false,
    )
    .await;
    let run = session
        .submit("go", "submit", Limits::default(), None)
        .await
        .unwrap();
    let children = wait_for_children(&run).await;
    let child = run.subagent(&children[0].session_id).await.unwrap();
    let grandchildren = child
        .delegate(vec![task("nested")], "nested")
        .await
        .unwrap();
    let grandchild = child.subagent(&grandchildren[0].session_id).await.unwrap();
    assert_eq!(
        grandchild
            .delegate(vec![task("too deep")], "deep")
            .await
            .unwrap_err()
            .code,
        "agent_limit"
    );
    run.cancel("stop", "stop").await.unwrap();
    run.wait().await.unwrap();
    assert!(grandchild.status().await.unwrap().status.terminal());
    assert_eq!(engine.execution_status()["free_subagent_slots"], 4);
    let child_session = engine.open_session(&children[0].session_id).await.unwrap();
    assert_eq!(
        child_session
            .configure(profile(false), HostPolicy::default(), "child-config")
            .await
            .unwrap_err()
            .code,
        "root_required"
    );
    // Configuration must inspect durable child links without deadlocking the
    // admission lock used to reopen those child sessions.
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        session.configure(profile(false), HostPolicy::default(), "root-config"),
    )
    .await
    .unwrap()
    .unwrap();
    engine.close().await.unwrap();
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn revoking_parent_grants_denies_child_writes() {
    let (engine, session, dir) = setup(
        Probe::new(Some(4), 200, true),
        ExecutionConfig::default(),
        true,
    )
    .await;
    session
        .enable_tool_approvals("enable-requests")
        .await
        .unwrap();
    let run = session
        .submit("go", "submit", Limits::default(), None)
        .await
        .unwrap();
    wait_for_children(&run).await;
    session.revoke_tool("files.write", "revoke").await.unwrap();
    run.wait().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
        "original"
    );
    for child in run.subagents().await.unwrap() {
        assert!(
            child
                .run
                .unwrap()
                .invocations
                .iter()
                .filter(|i| i.call.name == "files.write")
                .all(|i| i.result.as_ref().unwrap().status == Outcome::Denied)
        );
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn durable_child_links_survive_reopen_and_cannot_bypass_admission_by_resubmit() {
    use werk_worker::sessions::{SessionStore, SqliteStore};
    let workspace = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let path = storage.path().join("sessions.sqlite");
    let engine = WorkerEngine::builder(Probe::new(Some(4), 10, false))
        .store(Arc::new(SqliteStore::open(&path).await.unwrap()))
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(false),
            workspace.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let sid = session.id().await;
    let run = session
        .submit("go", "root", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let child_id = run.subagents().await.unwrap()[0].session_id.clone();
    let run_id = run.id.clone();
    engine.close().await.unwrap();
    let store = Arc::new(SqliteStore::open(&path).await.unwrap());
    // Explicit interrupted-state fixture; this does not claim an OS crash test.
    let mut child = store.load(&child_id).await.unwrap();
    let revision = child.revision;
    let child_run_id = child.runs[0].id.clone();
    child.runs[0].status = RunStatus::Interrupted;
    child.revision += 1;
    store.save(&child, Some(revision)).await.unwrap();
    let provider = Probe::new(Some(4), 10, false);
    let reopened = WorkerEngine::builder(provider.clone())
        .store(store)
        .build()
        .await
        .unwrap();
    let root = reopened
        .open_session(&sid)
        .await
        .unwrap()
        .run(&run_id)
        .await
        .unwrap();
    assert_eq!(root.subagents().await.unwrap().len(), 2);
    assert_eq!(
        root.subagent(&child_id)
            .await
            .unwrap()
            .status()
            .await
            .unwrap()
            .status,
        RunStatus::Interrupted
    );
    assert_eq!(
        reopened
            .resume_run(&child_id, &child_run_id)
            .await
            .err()
            .unwrap()
            .code,
        "agent_recovery"
    );
    let child = reopened.open_session(&child_id).await.unwrap();
    assert_eq!(
        child
            .submit("again", "new", Limits::default(), None)
            .await
            .err()
            .unwrap()
            .code,
        "agent_one_shot"
    );
    assert!(provider.seen.lock().unwrap().is_empty());
    reopened.close().await.unwrap();
}
