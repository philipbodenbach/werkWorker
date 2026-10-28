use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use werk_worker::{
    agents::DelegateTask,
    parallel::{DelegationLevel, ExecutionConfig},
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
        let collected = request.messages.iter().any(|m| {
            m.content.starts_with("Subagent results")
                || m.content.contains("\"event\":\"subagent_results\"")
        });
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
        // These probes exercise delegation/permission mechanics with legacy
        // replacement writes, not the coding acceptance contract.
        .harness(werk_worker::harness::HarnessConfig {
            coding_checks: false,
            ..Default::default()
        })
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
async fn turning_delegation_off_blocks_new_children_but_existing_children_finish() {
    let provider = Probe::new(Some(4), 300, false);
    let (engine, session, _dir) = setup(provider, ExecutionConfig::default(), false).await;
    session
        .set_delegation_level(DelegationLevel::Conservative, "initial")
        .await
        .unwrap();
    let run = session
        .submit("go", "start", Limits::default(), None)
        .await
        .unwrap();
    let children = wait_for_children(&run).await;
    session
        .set_delegation_level(DelegationLevel::Off, "off")
        .await
        .unwrap();
    assert_eq!(
        run.delegate(vec![task("new")], "new-child")
            .await
            .unwrap_err()
            .code,
        "delegation_disabled"
    );
    let child = engine.open_session(&children[0].session_id).await.unwrap();
    assert_eq!(
        DelegationLevel::preference(&child.snapshot().await.unwrap().metadata).unwrap(),
        DelegationLevel::Conservative
    );
    assert_eq!(
        child
            .set_delegation_level(DelegationLevel::Max, "escape")
            .await
            .unwrap_err()
            .code,
        "root_required"
    );
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert!(
        run.subagents()
            .await
            .unwrap()
            .iter()
            .all(|c| c.run.as_ref().unwrap().status == RunStatus::Completed)
    );
    session
        .set_delegation_level(DelegationLevel::Max, "max")
        .await
        .unwrap();
    assert!(matches!(
        session
            .set_delegation_level(DelegationLevel::Off, "off")
            .await
            .unwrap()
            .status,
        ReceiptStatus::Duplicate
    ));
    assert_eq!(
        DelegationLevel::from_metadata(&session.snapshot().await.unwrap().metadata).unwrap(),
        DelegationLevel::Max
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn explicit_project_delegation_always_wins_over_host_preferences() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Probe::new(Some(2), 1, false);
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    for fixed in DelegationLevel::ALL {
        let metadata = json!({"project_config":{"delegation_level":fixed,"model":"keep"}});
        let session = engine
            .create_session(
                profile(false),
                dir.path(),
                HostPolicy::default(),
                metadata.clone(),
            )
            .await
            .unwrap();
        for preference in DelegationLevel::ALL {
            session
                .set_delegation_level(preference, preference.label())
                .await
                .unwrap();
            let saved = session.snapshot().await.unwrap();
            assert_eq!(saved.metadata["project_config"], metadata["project_config"]);
            assert_eq!(
                DelegationLevel::from_metadata(&saved.metadata).unwrap(),
                fixed
            );
            assert_eq!(
                DelegationLevel::preference(&saved.metadata).unwrap(),
                preference
            );
        }
        if fixed == DelegationLevel::Off {
            let run = session
                .submit("go", "start", Limits::default(), None)
                .await
                .unwrap();
            run.wait().await.unwrap();
            assert!(run.subagents().await.unwrap().is_empty());
            let seen = provider.seen.lock().unwrap();
            assert!(!seen[0].tools.iter().any(|t| t.name == "delegate_task"));
            assert!(seen[0].tools.iter().any(|t| t.name == "agents.status"));
        }
    }
    engine.close().await.unwrap();
}

async fn wait_for_question(run: &RunHandle, question: &str) -> InteractionRequest {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = run.status().await.unwrap();
            if let Some(request) = status
                .invocations
                .iter()
                .filter_map(|i| i.interaction.as_ref())
                .find(|i| i.question == question && i.answer.is_none())
            {
                return request.clone();
            }
            assert!(
                !status.status.terminal(),
                "Run ended before {question}: {:?}",
                status.error
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn delegation_levels_start_helpers_at_distinct_stages_and_respect_capacity() {
    use werk_worker::policy::ToolPermission;
    for (level, capacity, allowed, expected_plan, expected_write) in [
        (None, 2, true, 0, 1), // Missing config defaults to Active.
        (Some(DelegationLevel::Off), 2, true, 0, 0),
        (Some(DelegationLevel::Conservative), 2, true, 0, 0),
        (Some(DelegationLevel::Active), 2, true, 0, 1),
        (Some(DelegationLevel::Max), 2, true, 1, 1),
        (Some(DelegationLevel::Max), 1, true, 0, 0),
        (Some(DelegationLevel::Max), 2, false, 0, 0),
    ] {
        let provider = Arc::new(CodingReviewProbe {
            capacity,
            root: ScriptedProvider::new(vec![
                tool_turn(
                    "plan",
                    "task.plan",
                    json!({"steps":[
                        {"id":"design","description":"Design","status":"in_progress"},
                        {"id":"build","description":"Build","status":"pending"},
                        {"id":"test","description":"Test","status":"pending"}
                    ]}),
                ),
                tool_turn(
                    "planned",
                    "interaction.ask",
                    json!({"question":"planned?","choices":["continue"]}),
                ),
                tool_turn(
                    "write",
                    "files.write",
                    json!({"path":"candidate.txt","content":"candidate"}),
                ),
                tool_turn(
                    "written",
                    "interaction.ask",
                    json!({"question":"written?","choices":["continue"]}),
                ),
            ]),
            child: ScriptedProvider::new(vec![ScriptedTurn::text(
                "Check the public behavior and edge cases.",
            )]),
            seen: Mutex::new(vec![]),
        });
        let engine = WorkerEngine::builder(provider).build().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let session = engine
            .create_session(
                AgentProfile {
                    name: "coding-basic".into(),
                    schema_version: 1,
                    prompt: "Implement and verify".into(),
                    skills: vec![],
                    tools: [
                        "task.plan",
                        "files.read",
                        "files.write",
                        "interaction.ask",
                        "delegate_task",
                    ]
                    .map(str::to_owned)
                    .to_vec(),
                },
                dir.path(),
                HostPolicy {
                    workspace_writable: true,
                    tool_permissions: [
                        ("files.write".into(), ToolPermission::Allow),
                        ("files.read".into(), ToolPermission::Allow),
                        (
                            "delegate_task".into(),
                            if allowed {
                                ToolPermission::Allow
                            } else {
                                ToolPermission::Deny
                            },
                        ),
                    ]
                    .into(),
                    ..Default::default()
                },
                json!({"project_config":{"delegation_level":level}}),
            )
            .await
            .unwrap();
        let run = session
            .submit(
                "Build a small application",
                "start",
                Limits::continuous(),
                None,
            )
            .await
            .unwrap();
        let question = wait_for_question(&run, "planned?").await;
        assert_eq!(
            run.subagents().await.unwrap().len(),
            expected_plan,
            "{level:?} / {capacity} / {allowed}"
        );
        run.respond_to_interaction(&question.request_id, "continue", "continue")
            .await
            .unwrap();
        wait_for_question(&run, "written?").await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("candidate.txt")).unwrap(),
            "candidate"
        );
        let children = run.subagents().await.unwrap();
        assert_eq!(
            children.len(),
            expected_write,
            "{level:?} / {capacity} / {allowed}"
        );
        for child in children {
            let data = engine
                .open_session(&child.session_id)
                .await
                .unwrap()
                .snapshot()
                .await
                .unwrap();
            assert!(!data.policy.workspace_writable);
            assert_eq!(data.profile.tools, vec!["files.read"]);
            assert_eq!(
                DelegationLevel::from_metadata(&data.metadata).unwrap(),
                level.unwrap_or_default()
            );
            let limits = &child.run.as_ref().unwrap().limits;
            assert!(!limits.continuous);
            assert_eq!(limits.max_model_requests, 6);
        }
        engine.close().await.unwrap();
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
    run.delegate(vec![task("extra")], "review_extra")
        .await
        .unwrap();
    run.delegate(vec![task("extra")], "review_extra")
        .await
        .unwrap();
    let children = run.subagents().await.unwrap();
    assert_eq!(children.len(), 3);
    let explicit = children.iter().find(|child| child.goal == "extra").unwrap();
    assert_eq!(
        explicit.run.as_ref().unwrap().limits.max_model_requests,
        Limits::default().max_model_requests
    );
    assert_eq!(
        run.delegate(vec![task("changed")], "review_extra")
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
    session
        .set_delegation_level(DelegationLevel::Off, "disable-tree")
        .await
        .unwrap();
    assert_eq!(
        child
            .delegate(vec![task("blocked descendant")], "blocked")
            .await
            .unwrap_err()
            .code,
        "delegation_disabled"
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
        let child_run = child.run.unwrap();
        assert!(
            child_run
                .invocations
                .iter()
                .filter(|i| i.call.name == "files.write")
                .all(|i| i.result.as_ref().unwrap().status == Outcome::Denied),
            "{:?}",
            child_run.invocations
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

#[tokio::test]
async fn minimal_children_inherit_behavior_and_results_keep_harness_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Probe::new(Some(4), 20, false);
    let engine = WorkerEngine::builder(provider.clone())
        .harness(harness::HarnessConfig {
            completion: Some(harness::CompletionCheck {
                tool: "agents.status".into(),
                arguments: json!({}),
                result_schema: json!({"required":["agents"]}),
                max_corrections: 0,
            }),
            ..harness::HarnessConfig::minimal()
        })
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(false),
            dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("Parent task", "start", Limits::default(), None)
        .await
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(result.text.as_deref(), Some("synthesized"));
    for child in run.subagents().await.unwrap() {
        let data = engine
            .open_session(&child.session_id)
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap();
        assert_eq!(
            harness::config(&data, &data.runs[0]).unwrap().mode,
            harness::HarnessMode::Adaptive
        );
        assert!(
            harness::config(&data, &data.runs[0])
                .unwrap()
                .completion
                .is_none()
        );
    }
    let data = session.snapshot().await.unwrap();
    assert_eq!(
        data.messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count(),
        1
    );
    let message = data
        .messages
        .iter()
        .position(|m| m.content.contains("\"event\":\"subagent_results\""))
        .unwrap();
    assert_eq!(data.messages[message].role, Role::Assistant);
    assert!(
        data.metadata["_werkworker_harness"][&run.id]["messages"]
            .as_array()
            .unwrap()
            .contains(&json!(message))
    );
    context::validate_messages(&data.messages).unwrap();
    engine.close().await.unwrap();
}

struct CodingReviewProbe {
    capacity: usize,
    root: ScriptedProvider,
    child: ScriptedProvider,
    seen: Mutex<Vec<ModelRequest>>,
}

struct LiveReviewProbe {
    parent_requests: AtomicUsize,
    parent_started: tokio::sync::Notify,
    release_parent: tokio::sync::Notify,
    review_received: tokio::sync::Notify,
    release_slow: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl ModelProvider for LiveReviewProbe {
    fn identity(&self) -> String {
        "live-review-fixture".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn concurrency(&self) -> InferenceConcurrency {
        InferenceConcurrency {
            max_requests: Some(3),
            source: "fixture".into(),
            detail: "verified".into(),
        }
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let child = request
            .messages
            .iter()
            .any(|m| m.role == Role::System && m.content.contains("delegated worker"));
        let slow = child
            && request
                .messages
                .iter()
                .any(|m| m.role == Role::User && m.content.contains("slow review"));
        let turn = if child {
            ScriptedTurn::text(if slow {
                "slow evidence"
            } else {
                "ready evidence"
            })
        } else {
            let n = self.parent_requests.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                self.parent_started.notify_one();
                self.release_parent.notified().await;
                tool_turn("inspect", "files.list", json!({"path":"."}))
            } else if n == 1 {
                let feedback: Vec<Value> = request
                    .messages
                    .iter()
                    .filter_map(|m| serde_json::from_str(&m.content).ok())
                    .filter(|v: &Value| v["event"] == "subagent_results")
                    .collect();
                assert_eq!(
                    feedback.len(),
                    1,
                    "completed review must arrive before parent final"
                );
                assert_eq!(feedback[0]["data"]["agents"].as_array().unwrap().len(), 1);
                assert_eq!(feedback[0]["data"]["agents"][0]["text"], "ready evidence");
                self.review_received.notify_one();
                tool_turn("continue", "files.list", json!({"path":"."}))
            } else {
                ScriptedTurn::text("Synthesis after work")
            }
        };
        let release = self.release_slow.clone();
        Ok(Box::pin(async_stream::stream! {
            if slow { release.notified().await; }
            for delta in turn.deltas { yield Ok(delta); }
        }))
    }
}

#[tokio::test]
async fn completed_child_arrives_during_work_once_without_waiting_for_running_sibling() {
    let provider = Arc::new(LiveReviewProbe {
        parent_requests: AtomicUsize::new(0),
        parent_started: tokio::sync::Notify::new(),
        release_parent: tokio::sync::Notify::new(),
        review_received: tokio::sync::Notify::new(),
        release_slow: Arc::new(tokio::sync::Notify::new()),
    });
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut agent = profile(false);
    agent.tools.push("files.list".into());
    let session = engine
        .create_session(
            agent,
            dir.path(),
            HostPolicy {
                tool_permissions: [(
                    "files.list".into(),
                    werk_worker::policy::ToolPermission::Allow,
                )]
                .into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Implement while reviews run",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.parent_started.notified(),
    )
    .await
    .unwrap();
    let children = run
        .delegate(
            vec![
                DelegateTask {
                    goal: "fast review".into(),
                    context: String::new(),
                    tools: Some(vec![]),
                    read_only: true,
                },
                DelegateTask {
                    goal: "slow review".into(),
                    context: String::new(),
                    tools: Some(vec![]),
                    read_only: true,
                },
            ],
            "reviews",
        )
        .await
        .unwrap();
    let ready = run.subagent(&children[0].session_id).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), ready.wait())
        .await
        .unwrap()
        .unwrap();
    provider.release_parent.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.review_received.notified(),
    )
    .await
    .unwrap();
    assert!(
        !run.subagent(&children[1].session_id)
            .await
            .unwrap()
            .status()
            .await
            .unwrap()
            .status
            .terminal()
    );
    provider.release_slow.notify_one();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), run.wait())
            .await
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Completed
    );
    let saved = session.snapshot().await.unwrap();
    context::validate_messages(&saved.messages).unwrap();
    let mut delivered = vec![];
    for message in &saved.messages {
        if let Ok(value) = serde_json::from_str::<Value>(&message.content)
            && value["event"] == "subagent_results"
        {
            assert_eq!(message.role, Role::Assistant);
            delivered.extend(
                value["data"]["agents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a["session_id"].as_str().unwrap().to_owned()),
            );
        }
    }
    delivered.sort();
    let mut expected: Vec<_> = children.iter().map(|c| c.session_id.clone()).collect();
    expected.sort();
    assert_eq!(delivered, expected);
    engine.close().await.unwrap();
}
#[async_trait]
impl ModelProvider for CodingReviewProbe {
    fn identity(&self) -> String {
        "coding-review-fixture".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn concurrency(&self) -> InferenceConcurrency {
        InferenceConcurrency {
            max_requests: Some(self.capacity),
            source: "fixture".into(),
            detail: "configured".into(),
        }
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let child = request
            .messages
            .iter()
            .any(|m| m.role == Role::System && m.content.contains("delegated worker"));
        self.seen.lock().unwrap().push(request.clone());
        if child {
            self.child.stream(request).await
        } else {
            self.root.stream(request).await
        }
    }
}
#[tokio::test]
async fn coding_review_uses_verified_spare_capacity_but_cannot_replace_tests() {
    use werk_worker::policy::ToolPermission;
    if let Err(error) = werk_worker::runtime::Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    for (capacity, completed, no_op, looping, checked) in [
        (1, false, false, false, true),
        (2, false, false, false, false),
        (2, false, false, false, true),
        (2, true, false, false, true),
        (2, false, true, false, true),
        (2, false, false, true, true),
    ] {
        let plan = json!({"steps":[
            {"id":"shell","description":"Create shell","status":"completed"},
            {"id":"behavior","description":"Implement behavior","status":if completed { "completed" } else { "in_progress" }},
            {"id":"verify","description":"Verify behavior","status":if completed { "completed" } else { "pending" }}
        ]});
        let mut finished_plan = plan.clone();
        for step in finished_plan["steps"].as_array_mut().unwrap() {
            step["status"] = json!("completed");
        }
        let mut turns = vec![
            tool_turn("plan", "task.plan", plan),
            tool_turn(
                "write",
                if no_op { "files.edit" } else { "files.write" },
                if no_op {
                    json!({"path":"game.txt","old":"incomplete","new":"incomplete","expected_hash":hash("incomplete")})
                } else {
                    json!({"path":"game.txt","content":"incomplete"})
                },
            ),
        ];
        if checked {
            turns.push(tool_turn("check-version", "process.run", json!({
                "argv":["/bin/sh","-c","test \"$(cat game.txt)\" = incomplete"],
                "verification":{"criteria":["The initial fixture state is persisted"],"paths":["game.txt"]}
            })));
            // Review of a passing first candidate cannot replace a later failed
            // acceptance check or turn the remaining task into a success.
            turns.push(tool_turn("remaining-check", "process.run", json!({
                "argv":["/bin/sh","-c","test \"$(cat game.txt)\" = complete"],
                "verification":{"criteria":["The complete fixture state is persisted"],"paths":["game.txt"]}
            })));
        }
        turns.push(tool_turn("finished-plan", "task.plan", finished_plan));
        turns.extend((0..6).map(|_| ScriptedTurn::text("Everything is done")));
        let provider = Arc::new(CodingReviewProbe {
            capacity,
            root: ScriptedProvider::new(turns),
            child: ScriptedProvider::new(if looping {
                (0..10)
                    .map(|offset| {
                        tool_turn(
                            &format!("inspect_{offset}"),
                            "files.read",
                            json!({"path":"game.txt","start_byte":offset}),
                        )
                    })
                    .collect()
            } else {
                vec![
                    tool_turn("inspect", "files.read", json!({"path":"game.txt"})),
                    ScriptedTurn::text("The implementation lacks a behavior test."),
                ]
            }),
            seen: Mutex::new(vec![]),
        });
        let engine = WorkerEngine::builder(provider.clone())
            .build()
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        if no_op {
            std::fs::write(dir.path().join("game.txt"), "incomplete").unwrap();
        }
        let session = engine
            .create_session(
                AgentProfile {
                    name: "coding-basic".into(),
                    schema_version: 1,
                    prompt: "Implement and verify".into(),
                    skills: vec![],
                    tools: [
                        "files.read",
                        "files.write",
                        "files.edit",
                        "task.plan",
                        "delegate_task",
                        "process.run",
                    ]
                    .map(str::to_owned)
                    .to_vec(),
                },
                dir.path(),
                HostPolicy {
                    workspace_writable: true,
                    tool_permissions: ["files.read", "files.write", "files.edit", "process.run"]
                        .map(|n| (n.into(), ToolPermission::Allow))
                        .into(),
                    ..Default::default()
                },
                json!({"project_config":{"delegation_level":"conservative"}}),
            )
            .await
            .unwrap();
        let run = session
            .submit("Build game", "start", Limits::continuous(), None)
            .await
            .unwrap();
        run.steer(
            "Keep keyboard controls and use one file",
            "updated-requirements",
        )
        .await
        .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.status, RunStatus::Failed, "{:?}", result.error);
        assert_eq!(result.error.unwrap().code, "completion_stalled");
        {
            let requests = provider.seen.lock().unwrap();
            let root = requests
                .iter()
                .find(|request| {
                    !request.messages.iter().any(|message| {
                        message.role == Role::System && message.content.contains("delegated worker")
                    })
                })
                .unwrap();
            let delegation = root
                .tools
                .iter()
                .find(|tool| tool.name == "delegate_task")
                .unwrap();
            assert!(
                delegation
                    .description
                    .contains(&format!("Effective configured inference slots: {capacity}"))
            );
            assert!(delegation.description.contains(&format!(
                "Recommended total active helpers for this parent: at most {}",
                capacity - 1
            )));
        }
        if no_op {
            let snapshot = run.status().await.unwrap();
            let edit = snapshot
                .invocations
                .iter()
                .find(|invocation| invocation.call.name == "files.edit")
                .unwrap();
            let receipt = edit.result.as_ref().unwrap();
            assert_eq!(receipt.status, Outcome::Succeeded, "{receipt:?}");
            assert_eq!(receipt.data["sha256"], hash("incomplete"));
            assert_eq!(
                std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
                "incomplete"
            );
        }
        let children = run.subagents().await.unwrap();
        assert_eq!(
            children.len(),
            usize::from(capacity == 2 && !completed && !no_op && checked)
        );
        if let Some(child) = children.first() {
            let review = child.run.as_ref().unwrap();
            assert!(!review.limits.continuous);
            assert_eq!(review.limits.max_model_requests, 6);
            assert_eq!(review.limits.max_tool_invocations, 8);
            assert!(review.metrics.model_requests <= 6);
            if looping {
                assert_eq!(review.status, RunStatus::Failed);
                assert_eq!(review.error.as_ref().unwrap().code, "request_budget");
                assert_eq!(review.metrics.model_requests, 6);
            } else {
                assert_eq!(review.status, RunStatus::Completed);
            }
            let saved = engine
                .open_session(&child.session_id)
                .await
                .unwrap()
                .snapshot()
                .await
                .unwrap();
            assert!(!saved.policy.workspace_writable);
            assert_eq!(saved.profile.tools, vec!["files.read"]);
            assert!(review.input.contains("Latest file receipts"));
            assert!(
                review
                    .input
                    .contains("Keep keyboard controls and use one file")
            );
            assert!(review.input.contains(&hash("incomplete")));
            assert!(review.input.contains("game.txt"));
        }
        assert!(result.text.is_none());
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn code_created_only_by_a_child_process_requires_parent_verification() {
    use werk_worker::policy::ToolPermission;
    if let Err(error) = werk_worker::runtime::Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let mut turns = vec![tool_turn(
        "delegate",
        "delegate_task",
        json!({"tasks":[{"goal":"Implement core","tools":["process.run"]}]}),
    )];
    turns.extend((0..6).map(|_| ScriptedTurn::text("Done")));
    let provider = Arc::new(CodingReviewProbe {
        capacity: 2,
        root: ScriptedProvider::new(turns),
        child: ScriptedProvider::new(vec![
            tool_turn(
                "shell",
                "process.run",
                json!({"argv":["/bin/sh","-c","printf incomplete > game.txt"]}),
            ),
            ScriptedTurn::text("Created code"),
        ]),
        seen: Mutex::new(vec![]),
    });
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                name: "coding-basic".into(),
                schema_version: 1,
                prompt: "Implement and verify".into(),
                skills: vec![],
                tools: vec!["delegate_task".into(), "process.run".into()],
            },
            dir.path(),
            HostPolicy {
                workspace_writable: true,
                tool_permissions: [("process.run".into(), ToolPermission::Allow)].into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("Build game", "start", Limits::continuous(), None)
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "incomplete"
    );
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().code, "completion_stalled");
    assert!(result.text.is_none());
    engine.close().await.unwrap();
}
