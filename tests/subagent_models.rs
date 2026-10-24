//! Multi-model delegation through one authenticated Werk origin. No live model.
mod support;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::*;
use werk_worker::{
    parallel::ExecutionConfig, policy::HostPolicy, profiles::AgentProfile, project::Project,
    providers::*, sessions::SqliteStore, *,
};

fn config(origin: &str) -> WerkConfig {
    let mut config = WerkConfig::local("main");
    config.base_url = origin.into();
    config.stream = false;
    config.deployment = Some("main-deployment".into());
    config.capabilities = ModelCapabilities {
        context_tokens: Some(32768),
        tool_calling: Support::Supported,
        ..Default::default()
    };
    config
        .options
        .insert("reasoning_effort".into(), json!("high"));
    config
}

fn reply(message: Value) -> Response {
    let finish = if message["tool_calls"].is_array() {
        "tool_calls"
    } else {
        "stop"
    };
    Response::json(
        json!({"choices":[{"message":message,"finish_reason":finish}],
        "usage":{"prompt_tokens":64,"completion_tokens":8}}),
    )
}

fn fixture(request: &Request) -> Response {
    match request.path.as_str() {
        "/werk/v1/capabilities" => control(json!({"capabilities":[
            {"id":"api.chat.limits","status":"supported","detail":"fixture","operations":["limits"]},
            {"id":"api.chat.token_count","status":"supported","detail":"fixture","operations":["count_tokens"]}
        ]})),
        "/v1/models" => Response::json(json!({"data":[{"id":"main"},{"id":"helper"}]})),
        path if path.starts_with("/werk/v1/chat/limits?") => {
            let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
            let model = url
                .query_pairs()
                .find(|(key, _)| key == "model")
                .unwrap()
                .1
                .into_owned();
            if !["main", "helper"].contains(&model.as_str()) {
                return Response {
                    status: 404,
                    headers: vec![],
                    body: br#"{"error":"model not found"}"#.to_vec(),
                };
            }
            control(json!({"schema_version":1,"model":model,
                "context_tokens":if model=="helper" {8192} else {32768},
                "max_output_tokens":if model=="helper" {1024} else {4096},
                "max_concurrent_requests":if model=="helper" {1} else {2}}))
        }
        "/werk/v1/chat/count_tokens" => {
            control(json!({"model":request.body["model"],"input_tokens":64}))
        }
        "/v1/chat/completions" => {
            let messages = request.body["messages"].as_array().unwrap();
            let child = messages.iter().any(|m| {
                m["role"] == "system"
                    && m["content"]
                        .as_str()
                        .is_some_and(|s| s.contains("delegated worker"))
            });
            let delegated = messages.iter().any(|m| {
                m["role"] == "tool"
                    || m["content"]
                        .as_str()
                        .is_some_and(|s| s.contains("subagent_results"))
            });
            if !child && !delegated {
                reply(json!({"role":"assistant","content":"","tool_calls":[{
                    "id":"delegate","type":"function","function":{"name":"delegate_task",
                    "arguments":json!({"tasks":[{"goal":"Inspect alpha","read_only":true},{"goal":"Inspect beta","read_only":true}]}).to_string()}}]}))
            } else {
                reply(
                    json!({"role":"assistant","content":if child {"child finished"} else {"parent finished"}}),
                )
            }
        }
        _ => werk(request),
    }
}

fn profile() -> AgentProfile {
    AgentProfile {
        schema_version: 1,
        name: "orchestrator".into(),
        prompt: "Follow the assigned task.".into(),
        tools: vec!["delegate_task".into(), "agents.wait".into()],
        skills: vec![],
    }
}

#[tokio::test]
async fn nested_children_keep_the_configured_model() {
    let server = Server::start(|request| {
        if request.path == "/v1/chat/completions" {
            let messages = request.body["messages"].as_array().unwrap();
            let child = messages.iter().any(|m| m["content"].as_str().is_some_and(|s| s.contains("delegated worker")));
            let can_delegate = request.body["tools"].as_array().is_some_and(|tools| tools.iter().any(|t| t["function"]["name"]=="delegate_task"));
            if child && can_delegate && !messages.iter().any(|m| m["role"]=="tool") {
                return reply(json!({"role":"assistant","content":"","tool_calls":[{
                    "id":"nested","type":"function","function":{"name":"delegate_task",
                    "arguments":json!({"tasks":[{"goal":"Inspect one detail","read_only":true}]}).to_string()}}]}));
            }
        }
        fixture(request)
    }).await;
    let provider = WerkProvider::connect(config(&server.origin), None)
        .await
        .unwrap();
    let engine = WorkerEngine::builder(Arc::new(provider))
        .execution(ExecutionConfig {
            max_depth: 2,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let session = engine
        .create_session(
            profile(),
            root.path(),
            HostPolicy::default(),
            json!({"project_config":{"subagent_model":"helper"}}),
        )
        .await
        .unwrap();
    let run = session
        .submit("Delegate", "start", Limits::default(), None)
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    for child in run.subagents().await.unwrap() {
        let handle = run.subagent(&child.session_id).await.unwrap();
        let grandchildren = handle.subagents().await.unwrap();
        assert_eq!(grandchildren.len(), 1);
        assert_eq!(
            grandchildren[0].run.as_ref().unwrap().status,
            RunStatus::Completed
        );
        let grandchild = engine
            .open_session(&grandchildren[0].session_id)
            .await
            .unwrap();
        assert_eq!(
            grandchild
                .reasoning_options(&Limits::default())
                .await
                .unwrap()
                .model,
            "helper"
        );
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn automatic_review_uses_subagent_model_with_existing_limits_and_permissions() {
    let server = Server::start(|request| {
        if request.path=="/v1/chat/completions" && request.body["model"]=="main" {
            let messages=request.body["messages"].as_array().unwrap();
            let called = |name:&str| messages.iter().any(|m| m["tool_calls"].as_array().is_some_and(|calls| calls.iter().any(|c|c["function"]["name"]==name)));
            let next= if !called("task.plan") {
                Some(("task.plan",json!({"steps":[
                    {"id":"build","description":"Build","status":"in_progress"},
                    {"id":"test","description":"Test","status":"pending"},
                    {"id":"finish","description":"Finish","status":"pending"}]})))
            } else if !called("files.write") {
                Some(("files.write",json!({"path":"game.txt","content":"implementation"})))
            } else {None};
            return if let Some((name,args))=next {
                reply(json!({"role":"assistant","content":"","tool_calls":[{"id":name,"type":"function","function":{"name":name,"arguments":args.to_string()}}]}))
            } else {reply(json!({"role":"assistant","content":"Finished"}))};
        }
        fixture(request)
    }).await;
    let provider = WerkProvider::connect(config(&server.origin), None)
        .await
        .unwrap();
    let engine = WorkerEngine::builder(Arc::new(provider))
        .build()
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "coding-basic".into(),
                prompt: "Implement and verify".into(),
                tools: ["task.plan", "files.read", "files.write", "delegate_task"]
                    .map(str::to_owned)
                    .to_vec(),
                skills: vec![],
            },
            root.path(),
            HostPolicy {
                workspace_writable: true,
                tool_permissions: [
                    (
                        "files.read".into(),
                        werk_worker::policy::ToolPermission::Allow,
                    ),
                    (
                        "files.write".into(),
                        werk_worker::policy::ToolPermission::Allow,
                    ),
                ]
                .into(),
                ..Default::default()
            },
            json!({"project_config":{"subagent_model":"helper"}}),
        )
        .await
        .unwrap();
    let run = session
        .submit("Build game", "start", Limits::continuous(), None)
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().code, "completion_stalled");
    let children = run.subagents().await.unwrap();
    assert_eq!(children.len(), 1);
    let child = &children[0];
    let saved = engine
        .open_session(&child.session_id)
        .await
        .unwrap()
        .snapshot()
        .await
        .unwrap();
    assert!(!saved.policy.workspace_writable);
    assert_eq!(saved.profile.tools, vec!["files.read"]);
    let review = child.run.as_ref().unwrap();
    assert_eq!(review.status, RunStatus::Completed);
    assert!(!review.limits.continuous);
    assert_eq!(review.limits.max_model_requests, 6);
    assert_eq!(review.limits.max_tool_invocations, 8);
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path == "/v1/chat/completions" && r.body["model"] == "helper")
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn project_model_routes_children_only_and_survives_reopen() {
    for selected in [None, Some("main"), Some("helper")] {
        let server = Server::start(fixture).await;
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".werkworker")).unwrap();
        std::fs::write(
            root.path().join(".werkworker/config.json"),
            json!({"version":1,"subagent_model":selected}).to_string(),
        )
        .unwrap();
        let project = Project::load(root.path()).await.unwrap();
        let provider = WerkProvider::connect(config(&server.origin), Some("fixture-secret".into()))
            .await
            .unwrap();
        let store = Arc::new(
            SqliteStore::open(storage.path().join("sessions.sqlite"))
                .await
                .unwrap(),
        );
        let engine = WorkerEngine::builder(Arc::new(provider))
            .store(store.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                profile(),
                root.path(),
                HostPolicy::default(),
                project.scoped_metadata(),
            )
            .await
            .unwrap();
        let run = session
            .submit(
                "Delegate two investigations",
                "start",
                Limits::default(),
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(
            result.status,
            RunStatus::Completed,
            "{selected:?}: {:?}",
            result.error
        );
        let children = run.subagents().await.unwrap();
        assert_eq!(children.len(), 2);
        for child in &children {
            assert_eq!(child.run.as_ref().unwrap().status, RunStatus::Completed);
            let handle = engine.open_session(&child.session_id).await.unwrap();
            assert!(!handle.snapshot().await.unwrap().policy.workspace_writable);
            assert_eq!(
                handle.execution_status()["context_tokens"],
                if selected == Some("helper") {
                    8192
                } else {
                    32768
                }
            );
        }
        let requests = server.requests.lock().unwrap().clone();
        let mut child_requests = 0;
        for request in requests.iter().filter(|r| r.path == "/v1/chat/completions") {
            assert!(request.raw.contains("Bearer fixture-secret"));
            let child = request.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| {
                    m["content"]
                        .as_str()
                        .is_some_and(|s| s.contains("delegated worker"))
                });
            if child {
                child_requests += 1;
                assert_eq!(request.body["model"], selected.unwrap_or("main"));
            } else {
                assert_eq!(request.body["model"], "main");
                assert_eq!(request.body["reasoning_effort"], "high");
            }
            if request.body["model"] == "helper" {
                assert!(request.body["max_completion_tokens"].as_u64().unwrap() <= 1024);
                assert!(request.body.get("reasoning_effort").is_none());
                assert!(!request.raw.contains("main-deployment"));
            }
        }
        assert_eq!(child_requests, 2);
        assert_eq!(
            run.status().await.unwrap().tree_model_requests as usize,
            requests
                .iter()
                .filter(|r| r.path == "/v1/chat/completions")
                .count()
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.path == "/werk/v1/chat/limits?model=helper")
                .count(),
            usize::from(selected == Some("helper"))
        );
        if selected == Some("helper") {
            assert!(
                requests
                    .iter()
                    .any(|r| r.path == "/werk/v1/chat/count_tokens" && r.body["model"] == "helper")
            );
        }
        engine.close().await.unwrap();
        // Reopening uses the session snapshot, even after editing project defaults.
        std::fs::write(
            root.path().join(".werkworker/config.json"),
            r#"{"subagent_model":"missing"}"#,
        )
        .unwrap();
        let provider = WerkProvider::connect(config(&server.origin), Some("fixture-secret".into()))
            .await
            .unwrap();
        let engine = WorkerEngine::builder(Arc::new(provider))
            .store(Arc::new(
                SqliteStore::open(storage.path().join("sessions.sqlite"))
                    .await
                    .unwrap(),
            ))
            .build()
            .await
            .unwrap();
        let reopened = engine.open_session(&children[0].session_id).await.unwrap();
        assert_eq!(
            reopened
                .reasoning_options(&Limits::default())
                .await
                .unwrap()
                .model,
            selected.unwrap_or("main")
        );
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn unavailable_model_is_reported_without_falling_back() {
    let server = Server::start(fixture).await;
    let provider = WerkProvider::connect(config(&server.origin), None)
        .await
        .unwrap();
    let engine = WorkerEngine::builder(Arc::new(provider))
        .build()
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let session = engine
        .create_session(
            profile(),
            root.path(),
            HostPolicy::default(),
            json!({"project_config":{"subagent_model":"missing"}}),
        )
        .await
        .unwrap();
    let run = session
        .submit("Delegate", "start", Limits::default(), None)
        .await
        .unwrap();
    run.wait().await.unwrap();
    let state = run.status().await.unwrap();
    assert!(run.subagents().await.unwrap().is_empty());
    assert!(state.invocations.iter().any(|i| {
        i.call.name == "delegate_task"
            && i.result
                .as_ref()
                .is_some_and(|r| r.status != Outcome::Succeeded)
    }));
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .all(|r| !r.body.to_string().contains("delegated worker"))
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn explicit_subagent_model_keeps_the_shared_tree_budget() {
    let server = Server::start(fixture).await;
    let provider = WerkProvider::connect(config(&server.origin), None)
        .await
        .unwrap();
    let engine = WorkerEngine::builder(Arc::new(provider))
        .execution(ExecutionConfig {
            max_model_requests: 1,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let session = engine
        .create_session(
            profile(),
            root.path(),
            HostPolicy::default(),
            json!({"project_config":{"subagent_model":"helper"}}),
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Delegate",
            "start",
            Limits {
                max_model_requests: 2,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(run.status().await.unwrap().tree_model_requests, 2);
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        2
    );
    engine.close().await.unwrap();
}

#[derive(Default)]
struct ConcurrencyProbe {
    active: AtomicUsize,
    peak: AtomicUsize,
    children: AtomicUsize,
    child_peak: AtomicUsize,
}
struct RoutingProbe {
    model: &'static str,
    stats: Arc<ConcurrencyProbe>,
}
struct Inflight {
    stats: Arc<ConcurrencyProbe>,
    child: bool,
}
impl Drop for Inflight {
    fn drop(&mut self) {
        self.stats.active.fetch_sub(1, Ordering::SeqCst);
        if self.child {
            self.stats.children.fetch_sub(1, Ordering::SeqCst);
        }
    }
}
#[async_trait::async_trait]
impl ModelProvider for RoutingProbe {
    fn identity(&self) -> String {
        self.model.into()
    }
    fn model_id(&self) -> Option<&str> {
        Some(self.model)
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn concurrency(&self) -> InferenceConcurrency {
        InferenceConcurrency {
            max_requests: Some(if self.model == "helper" { 1 } else { 2 }),
            source: "test".into(),
            detail: "test".into(),
        }
    }
    async fn for_model(&self, model: &str) -> Result<Arc<dyn ModelProvider>> {
        assert_eq!(model, "helper");
        Ok(Arc::new(Self {
            model: "helper",
            stats: self.stats.clone(),
        }))
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let child = self.model == "helper";
        let delegated = request.messages.iter().any(|m| m.role == Role::Tool);
        let turn = if !child && !delegated {
            ScriptedTurn::calls(
                "",
                vec![ToolCall {
                    id: "delegate".into(),
                    name: "delegate_task".into(),
                    arguments: json!({"tasks":[{"goal":"First"},{"goal":"Second"}]}),
                }],
            )
        } else {
            ScriptedTurn::text("Finished")
        };
        let stats = self.stats.clone();
        Ok(Box::pin(async_stream::stream! {
            let _guard=Inflight {stats:stats.clone(),child};
            let active=stats.active.fetch_add(1,Ordering::SeqCst)+1;
            stats.peak.fetch_max(active,Ordering::SeqCst);
            if child {
                let active=stats.children.fetch_add(1,Ordering::SeqCst)+1;
                stats.child_peak.fetch_max(active,Ordering::SeqCst);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            for delta in turn.deltas {yield Ok(delta);}
        }))
    }
}

#[tokio::test]
async fn separate_models_share_host_capacity_and_each_model_keeps_its_own_limit() {
    for host_limit in [1, 2] {
        let stats = Arc::new(ConcurrencyProbe::default());
        let provider = Arc::new(RoutingProbe {
            model: "main",
            stats: stats.clone(),
        });
        let engine = WorkerEngine::builder(provider)
            .execution(ExecutionConfig {
                max_model_requests: host_limit,
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = engine
            .create_session(
                profile(),
                root.path(),
                HostPolicy::default(),
                json!({"project_config":{"subagent_model":"helper"}}),
            )
            .await
            .unwrap();
        let run = session
            .submit("Delegate", "start", Limits::default(), None)
            .await
            .unwrap();
        assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
        assert_eq!(stats.peak.load(Ordering::SeqCst), host_limit);
        assert_eq!(stats.child_peak.load(Ordering::SeqCst), 1);
        assert_eq!(stats.active.load(Ordering::SeqCst), 0);
        assert_eq!(session.execution_status()["free_model_slots"], host_limit);
        engine.close().await.unwrap();
    }
}
