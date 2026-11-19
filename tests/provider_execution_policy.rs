//! The provider can propose the same actions on both transports. Only the host can authorize them.
mod support;

use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use support::{Request, Response, Server, werk};
use werk_worker::{
    ApprovalDecision, EventKind, Limits, Outcome, RunStatus, WorkerEngine,
    harness::HarnessConfig,
    policy::{HostPolicy, ToolPermission},
    profiles::AgentProfile,
    providers::{Support, WerkConfig, WerkProvider},
};

fn respond(request: &Request) -> Response {
    match request.path.as_str() {
        "/v1/models" => Response::json(json!({"data":[{"id":"fixture-model"}]})),
        "/werk/v1/chat/completions" | "/v1/chat/completions" => {
            let count = request.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "tool")
                .count();
            let intent = match count {
                0 => Some(("files.read", json!({"path":"input.txt"}))),
                1 => Some((
                    "files.write",
                    json!({"path":"approved.txt","content":"authorized"}),
                )),
                2 => Some((
                    "files.write",
                    json!({"path":"denied.txt","content":"rejected"}),
                )),
                3 => Some((
                    "process.run",
                    json!({"argv":["unapproved-fixture"],"cwd":"."}),
                )),
                _ => None,
            };
            let choice = if let Some((name, args)) = intent {
                json!({"message":{"role":"assistant","content":null,"tool_calls":[{
                    "id":format!("intent_{count}"),"type":"function",
                    "function":{"name":name,"arguments":args.to_string()}
                }]},"finish_reason":"tool_calls"})
            } else {
                json!({"message":{"role":"assistant","content":"Finished."},"finish_reason":"stop"})
            };
            Response::json(json!({"choices":[choice]}))
        }
        _ => werk(request),
    }
}

async fn exercise(native: bool) -> Vec<(String, Outcome, Option<String>)> {
    let server = if native {
        Server::start(respond).await
    } else {
        Server::raw(respond).await
    };
    let mut config = WerkConfig::local("fixture-model");
    config.base_url = server.origin.clone();
    config.stream = false;
    config.capabilities.context_tokens = Some(32768);
    config.capabilities.tool_calling = Support::Supported;
    let credential = "synthetic-credential-kept-out-of-chat";
    let provider = if native {
        WerkProvider::connect(config, Some(credential.into())).await
    } else {
        WerkProvider::connect_openai_compatible(config, Some(credential.into())).await
    }
    .unwrap();
    let engine = WorkerEngine::builder(Arc::new(provider))
        .harness(HarnessConfig {
            coding_checks: false,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("private-host-workspace-marker");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("input.txt"), "permitted input").unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "policy-equivalence".into(),
                prompt: "Exercise the host tool policy.".into(),
                tools: ["files.read", "files.write", "process.run"]
                    .map(String::from)
                    .to_vec(),
                skills: vec![],
            },
            &workspace,
            HostPolicy {
                workspace_writable: true,
                tool_permissions: [
                    ("files.read".into(), ToolPermission::Allow),
                    ("files.write".into(), ToolPermission::Ask),
                    ("process.run".into(), ToolPermission::Deny),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Exercise the configured tools.",
            "policy-test",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    let mut approvals = vec![];
    let mut local_ids = vec![];
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut events = run.events(0);
        while let Some(event) = events.next().await {
            if let EventKind::ApprovalRequested { request } = event.unwrap().event {
                assert_eq!(request.description["tool"], "files.write");
                assert_eq!(request.run_id, run.id);
                let path = request.description["arguments"]["path"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                assert!(!workspace.join(&path).exists());
                let decision = match path.as_str() {
                    "approved.txt" => ApprovalDecision::AllowOnce,
                    "denied.txt" => ApprovalDecision::Deny,
                    other => panic!("unexpected approval {other}"),
                };
                run.respond_to_approval(
                    &request.request_id,
                    decision,
                    &format!("decision_{}", approvals.len()),
                )
                .await
                .unwrap();
                local_ids.extend([request.request_id, request.invocation_id]);
                approvals.push(path);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(approvals, ["approved.txt", "denied.txt"]);
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(
        std::fs::read(workspace.join("approved.txt")).unwrap(),
        b"authorized"
    );
    assert!(!workspace.join("denied.txt").exists());
    let snapshot = session.snapshot().await.unwrap();
    let state = snapshot.run(&run.id).unwrap();
    let outcomes: Vec<_> = state
        .invocations
        .iter()
        .map(|invocation| {
            let result = invocation.result.as_ref().unwrap();
            (
                invocation.call.name.clone(),
                result.status.clone(),
                result.error_code.clone(),
            )
        })
        .collect();
    assert_eq!(outcomes.len(), 4);
    assert_eq!(outcomes[0].1, Outcome::Succeeded);
    assert_eq!(outcomes[1].1, Outcome::Succeeded);
    assert_eq!(outcomes[2].1, Outcome::Denied);
    assert_eq!(outcomes[2].2.as_deref(), Some("approval_denied"));
    assert_eq!(outcomes[3].1, Outcome::Denied);
    assert_eq!(outcomes[3].2.as_deref(), Some("capability_denied"));
    assert!(
        !state.invocations[3]
            .result
            .as_ref()
            .unwrap()
            .data
            .get("executed")
            .is_some_and(|v| v == true)
    );
    {
        let requests = server.requests.lock().unwrap();
        let last = requests
            .iter()
            .rev()
            .find(|r| r.path.ends_with("/chat/completions"))
            .unwrap();
        assert_eq!(
            last.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "tool")
                .count(),
            4
        );
        for request in requests.iter().filter(|r| r.method == "POST") {
            let body = request.body.to_string();
            assert!(
                !body.contains("private-host-workspace-marker"),
                "host workspace path in provider request"
            );
            assert!(!body.contains(credential));
            for id in &local_ids {
                assert!(
                    !body.contains(id),
                    "host approval metadata in provider request"
                );
            }
        }
    }
    // Original persisted runs omitted harness metadata and replay the classic system prompt.
    #[cfg(any(windows, target_os = "macos"))]
    {
        use werk_worker::context::{ContextBuilder, DefaultContextBuilder};
        let mut legacy = snapshot;
        legacy.metadata = json!({});
        let (request, _) = DefaultContextBuilder
            .build(&legacy, legacy.run(&run.id).unwrap(), vec![])
            .await
            .unwrap();
        let system = &request.messages[0].content;
        assert!(system.contains("Native process platform"));
        assert!(
            !system.contains("private-host-workspace-marker"),
            "host workspace path in classic replay"
        );
    }
    engine.close().await.unwrap();
    outcomes
}

#[tokio::test]
async fn native_and_direct_providers_share_the_same_local_authority() {
    let native = exercise(true).await;
    let direct = exercise(false).await;
    assert_eq!(native, direct);
}
