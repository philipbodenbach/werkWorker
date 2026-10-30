//! Deadline cleanup through the public engine and real Werk HTTP transport.
mod support;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use support::*;
use werk_worker::{policy::HostPolicy, profiles::AgentProfile, providers::*, *};

fn configuration(server: &Server, streaming: bool, grace_ms: u64) -> WerkConfig {
    let mut config = WerkConfig::local("fixture-model");
    config.base_url = server.origin.clone();
    config.stream = streaming;
    config.timeout_ms = 5_000;
    config.deadline_drain_timeout_ms = grace_ms;
    config.capabilities.tool_calling = Support::Supported;
    config.capabilities.streaming = Support::Supported;
    config
}
fn discovery(request: &Request) -> Response {
    match request.path.as_str() {
        "/werk/v1/capabilities" => control(json!({"capabilities":[
            {"id":"api.chat.limits","status":"supported","operations":["limits"],"detail":"test capacity"}
        ]})),
        path if path.starts_with("/werk/v1/chat/limits?") => control(json!({
            "schema_version":1,"model":"fixture-model","context_tokens":32768,
            "max_output_tokens":8192,"max_concurrent_requests":2
        })),
        _ => werk(request),
    }
}
fn reply(request: &Request, message: Value, terminal: bool) -> Response {
    let finish = if message["tool_calls"].is_array() {
        "tool_calls"
    } else {
        "stop"
    };
    if request.body["stream"] == true {
        let mut body = format!("data: {}\n\n", json!({"choices":[{"delta":message}]}));
        if terminal {
            body.push_str(&format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{},"finish_reason":finish}]})
            ));
        }
        Response {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: body.into_bytes(),
        }
    } else {
        Response::json(json!({"choices":[{"message":message,"finish_reason":finish}]}))
    }
}
fn call(name: &str, arguments: Value) -> Value {
    json!({"role":"assistant","content":"","tool_calls":[{
        "index":0,"id":"call_fixture","type":"function",
        "function":{"name":name,"arguments":arguments.to_string()}
    }]})
}
fn profile(coding: bool) -> AgentProfile {
    AgentProfile {
        schema_version: 1,
        name: if coding { "coding-basic" } else { "test" }.into(),
        prompt: "Complete the task".into(),
        tools: if coding {
            vec![
                "task.plan".into(),
                "files.read".into(),
                "delegate_task".into(),
            ]
        } else {
            vec![]
        },
        skills: vec![],
    }
}
async fn event(run: &RunHandle, predicate: impl Fn(&EventKind) -> bool) -> WorkerEvent {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = run.events(0);
        while let Some(event) = events.next().await {
            let event = event.unwrap();
            if predicate(&event.event) {
                return event;
            }
        }
        panic!("expected event not emitted");
    })
    .await
    .expect("test event timeout")
}
async fn finish(run: &RunHandle) -> RunResult {
    tokio::time::timeout(Duration::from_secs(15), run.wait())
        .await
        .expect("run stalled")
        .unwrap()
}

#[tokio::test]
async fn automatic_review_deadline_keeps_parallel_parent_and_shared_provider_usable() {
    // Cover cancellation while waiting for headers, consuming SSE, and awaiting
    // a non-streaming response. No live model, subprocess or sandbox fixture.
    for (streaming, before_headers) in [(true, true), (true, false), (false, true)] {
        let server = Server::with_chat_delays(
            |request| {
                if request.path != "/v1/chat/completions" {
                    return discovery(request);
                }
                let messages = request.body["messages"].as_array().unwrap();
                if messages
                    .iter()
                    .any(|m| m["role"] == "user" && m["content"] == "Check the connection")
                {
                    return reply(
                        request,
                        json!({"role":"assistant","content":"Connection usable"}),
                        true,
                    );
                }
                let child = messages.iter().any(|m| {
                    m["role"] == "system"
                        && m["content"]
                            .as_str()
                            .is_some_and(|s| s.contains("delegated worker"))
                });
                let planned = messages.iter().any(|m| m["role"] == "tool");
                let message = if child {
                    // Even a complete late tool call must not execute.
                    call("files.read", json!({"path":"must-not-be-read.txt"}))
                } else if !planned {
                    call(
                        "task.plan",
                        json!({"steps":[
                            {"id":"build","description":"Build","status":"in_progress"},
                            {"id":"test","description":"Test","status":"pending"},
                            {"id":"finish","description":"Finish","status":"pending"}
                        ]}),
                    )
                } else {
                    json!({"role":"assistant","content":"Candidate ready"})
                };
                reply(request, message, true)
            },
            before_headers.then_some(Duration::from_millis(800)),
            (!before_headers).then_some((65536, Duration::from_millis(800))),
        )
        .await;
        let provider = Arc::new(
            WerkProvider::connect(configuration(&server, streaming, 2_000), None)
                .await
                .unwrap(),
        );
        let engine = WorkerEngine::builder(provider.clone())
            .build()
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let session = engine
            .create_session(
                profile(true),
                dir.path(),
                HostPolicy::default(),
                json!({"project_config":{"delegation_level":"max"}}),
            )
            .await
            .unwrap();
        let run = session
            .submit(
                "Build the page",
                "start",
                Limits {
                    continuous: true,
                    max_wall_ms: 400,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let child_id = match event(&run, |e| {
            matches!(
                e,
                EventKind::Subagent {
                    status: RunStatus::Queued,
                    ..
                }
            )
        })
        .await
        .event
        {
            EventKind::Subagent { session_id, .. } => session_id,
            _ => unreachable!(),
        };
        let child = run.subagent(&child_id).await.unwrap();
        event(
            &child,
            |e| matches!(e, EventKind::Diagnostic {code} if code == "model_deadline_draining"),
        )
        .await;
        assert!(!child.status().await.unwrap().status.terminal());
        assert!(
            run.status().await.unwrap().metrics.model_requests >= 2,
            "parent continues concurrently"
        );
        assert_eq!(
            session.execution_status()["free_model_slots"],
            0,
            "cleanup must retain the child's inference permit"
        );
        let review = finish(&child).await;
        assert_eq!(review.error.unwrap().code, "wall_budget");
        assert_eq!(review.metrics.model_requests, 1);
        assert_eq!(review.metrics.tool_invocations, 0);
        let child_data = engine
            .open_session(&child_id)
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap();
        assert!(!child_data.events.iter().any(|e| matches!(
            e.event,
            EventKind::ToolPrepared { .. }
                | EventKind::ToolDelta { .. }
                | EventKind::ModelCompleted { .. }
        )));
        let result = finish(&run).await;
        // The fixture has no implementation or verification: its normal coding
        // acceptance gate still applies. It must not fail with inference_unresolved.
        assert_eq!(result.error.unwrap().code, "completion_stalled");
        assert!(result.metrics.model_requests >= 3);
        let saved = session.snapshot().await.unwrap();
        assert!(saved.events.iter().any(|e| matches!(&e.event, EventKind::Subagent { error:Some(error), .. } if error.code == "wall_budget")));
        let other_dir = tempfile::tempdir().unwrap();
        let other = engine
            .create_session(
                profile(false),
                other_dir.path(),
                HostPolicy::default(),
                Value::Null,
            )
            .await
            .unwrap();
        // A fresh ordinary session sharing the very same provider also works.
        let followup = other
            .submit("Check the connection", "check", Limits::default(), None)
            .await
            .unwrap();
        assert_eq!(finish(&followup).await.status, RunStatus::Completed);
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn expired_request_cleanup_remains_bounded_and_uncertain_results_stay_blocked() {
    for terminal in [false, true] {
        let server = Server::with_chat_delays(
            move |r| {
                if r.path != "/v1/chat/completions" {
                    return discovery(r);
                }
                reply(
                    r,
                    json!({"role":"assistant","content":"late reply"}),
                    terminal,
                )
            },
            Some(Duration::from_millis(600)),
            None,
        )
        .await;
        let provider = Arc::new(
            WerkProvider::connect(
                configuration(&server, true, if terminal { 100 } else { 1500 }),
                None,
            )
            .await
            .unwrap(),
        );
        let engine = WorkerEngine::builder(provider.clone())
            .harness(werk_worker::harness::HarnessConfig {
                coding_checks: false,
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
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
            .submit(
                "Task",
                "start",
                Limits {
                    max_wall_ms: 200,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let result = finish(&run).await;
        let error = result.error.unwrap();
        assert_eq!(error.code, "wall_budget");
        if terminal {
            assert!(error.message.contains("cleanup did not finish"));
        }
        assert_eq!(result.metrics.tool_invocations, 0);
        let followup = session
            .submit("Continue", "followup", Limits::default(), None)
            .await
            .unwrap();
        assert_eq!(
            finish(&followup).await.error.unwrap().code,
            "inference_unresolved"
        );
        assert_eq!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.path == "/v1/chat/completions")
                .count(),
            1
        );
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn explicit_cancellation_interrupts_deadline_cleanup() {
    let server = Server::with_chat_delays(discovery, Some(Duration::from_secs(3)), None).await;
    let provider = Arc::new(
        WerkProvider::connect(configuration(&server, true, 5_000), None)
            .await
            .unwrap(),
    );
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
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
        .submit(
            "Task",
            "start",
            Limits {
                max_wall_ms: 200,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    event(
        &run,
        |e| matches!(e, EventKind::Diagnostic {code} if code=="model_deadline_draining"),
    )
    .await;
    run.cancel("User cancelled", "cancel").await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), run.wait())
        .await
        .expect("cancellation waited for the backend")
        .unwrap();
    assert_eq!(result.status, RunStatus::Cancelled);
    let followup = session
        .submit("Continue", "followup", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(
        finish(&followup).await.error.unwrap().code,
        "inference_unresolved"
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn engine_shutdown_interrupts_deadline_cleanup_and_keeps_uncertain_guard() {
    let server = Server::with_chat_delays(discovery, Some(Duration::from_secs(3)), None).await;
    let provider = Arc::new(
        WerkProvider::connect(configuration(&server, true, 5_000), None)
            .await
            .unwrap(),
    );
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
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
        .submit(
            "Task",
            "start",
            Limits {
                max_wall_ms: 200,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    event(
        &run,
        |e| matches!(e, EventKind::Diagnostic {code} if code=="model_deadline_draining"),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), engine.close())
        .await
        .expect("shutdown waited for the backend")
        .unwrap();
    let request = ModelRequest {
        reasoning_tier: None,
        messages: vec![Message::text(Role::User, "followup")],
        tools: vec![],
        max_output_tokens: 512,
        output_schema: None,
    };
    assert_eq!(
        provider.stream(request).await.err().unwrap().code,
        "inference_unresolved"
    );
}

#[tokio::test]
async fn deadline_while_waiting_for_capacity_never_dispatches_or_starts_cleanup() {
    let dispatched = Arc::new(tokio::sync::Notify::new());
    let server = Server::with_chat_delays(
        {
            let dispatched = dispatched.clone();
            move |request| {
                if request.path != "/v1/chat/completions" {
                    return discovery(request);
                }
                dispatched.notify_one();
                reply(
                    request,
                    json!({"role":"assistant","content":"Finished"}),
                    true,
                )
            }
        },
        Some(Duration::from_secs(2)),
        None,
    )
    .await;
    let provider = Arc::new(
        WerkProvider::connect(configuration(&server, true, 3_000), None)
            .await
            .unwrap(),
    );
    let engine = WorkerEngine::builder(provider)
        .execution(werk_worker::parallel::ExecutionConfig {
            max_model_requests: 1,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let first = engine
        .create_session(
            profile(false),
            first_dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let second = engine
        .create_session(
            profile(false),
            second_dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let active = first
        .submit("Use the slot", "first", Limits::continuous(), None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), dispatched.notified())
        .await
        .unwrap();
    let queued = second
        .submit(
            "Waiting",
            "second",
            Limits {
                max_wall_ms: 200,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), queued.wait())
        .await
        .expect("queued request entered cleanup")
        .unwrap();
    assert_eq!(result.error.unwrap().code, "wall_budget");
    assert!(!second.snapshot().await.unwrap().events.iter().any(
        |e| matches!(&e.event, EventKind::Diagnostic {code} if code=="model_deadline_draining")
    ));
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        1
    );
    assert_eq!(finish(&active).await.status, RunStatus::Completed);
    engine.close().await.unwrap();
}

#[test]
fn old_configuration_and_subagent_events_remain_readable() {
    let mut old = serde_json::to_value(WerkConfig::local("fixture-model")).unwrap();
    old.as_object_mut()
        .unwrap()
        .remove("deadline_drain_timeout_ms");
    let restored: WerkConfig = serde_json::from_value(old).unwrap();
    assert_eq!(restored.deadline_drain_timeout_ms, 600_000);
    let event: EventKind = serde_json::from_value(json!({"type":"subagent","data":{
        "session_id":"child","goal":"Review","status":"failed"
    }}))
    .unwrap();
    assert!(matches!(event, EventKind::Subagent { error: None, .. }));
    let mut invalid = restored;
    invalid.deadline_drain_timeout_ms = 600_001;
    assert_eq!(
        WerkClient::new(&invalid, None).unwrap_err().code,
        "invalid_limits"
    );
}
