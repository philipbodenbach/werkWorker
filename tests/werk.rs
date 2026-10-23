// Infrastructure fixtures explicitly opt out of coding acceptance checks.
// Behavioral completion is exercised in tests/completion.rs.
mod support;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use support::*;
use werk_worker::{
    policy::HostPolicy, profiles::AgentProfile, providers::*, tools::WerkNativeTool, *,
};

fn config(origin: &str, stream: bool) -> WerkConfig {
    WerkConfig {
        base_url: origin.into(),
        stream,
        capabilities: ModelCapabilities {
            context_tokens: Some(32768),
            tool_calling: Support::Supported,
            streaming: Support::Supported,
            ..Default::default()
        },
        ..WerkConfig::local("fixture-model")
    }
}

#[tokio::test]
async fn generation_limits_are_negotiated_without_probes_and_legacy_stays_unknown() {
    for advertised in [false, true] {
        let server = Server::start(move |request| match request.path.as_str() {
            "/werk/v1/capabilities" if advertised => control(json!({"capabilities":[
                {"id":"api.chat.limits","status":"supported","detail":"fixture","operations":["limits"]}]})),
            path if path.starts_with("/werk/v1/chat/limits?") => control(json!({
                "schema_version":1,"model":"fixture-model","context_tokens":16384,"max_output_tokens":8192})),
            _ => werk(request),
        }).await;
        let provider = WerkProvider::connect(config(&server.origin, false), None)
            .await
            .unwrap();
        assert_eq!(provider.max_output_tokens(), advertised.then_some(8192));
        if advertised {
            assert_eq!(provider.capabilities().context_tokens, Some(16384));
        }
        let requests = server.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .any(|r| r.path.starts_with("/werk/v1/chat/limits?")),
            advertised
        );
        assert!(
            !requests
                .iter()
                .any(|r| r.path == "/v1/chat/completions" || r.path == "/v1/capabilities")
        );
    }
}

#[tokio::test]
async fn invalid_generation_limits_fail_before_inference() {
    for patch in [
        json!({"model":"other"}),
        json!({"schema_version":2}),
        json!({"max_output_tokens":0}),
        json!({"max_output_tokens":32768}),
        json!({"context_tokens":0}),
    ] {
        let server = Server::start(move |request| match request.path.as_str() {
            "/werk/v1/capabilities" => control(json!({"capabilities":[
                {"id":"api.chat.limits","status":"supported","detail":"fixture","operations":["limits"]}]})),
            path if path.starts_with("/werk/v1/chat/limits?") => {
                let mut limits = json!({"schema_version":1,"model":"fixture-model","context_tokens":16384,"max_output_tokens":8192});
                limits.as_object_mut().unwrap().extend(patch.as_object().unwrap().clone());
                control(limits)
            },
            _ => werk(request),
        }).await;
        let error = WerkProvider::connect(config(&server.origin, false), None)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "invalid_generation_limits");
    }
}

#[tokio::test]
async fn request_fingerprints_include_native_reasoning_configuration_without_credentials() {
    let server = Server::start(werk).await;
    let mut low = config(&server.origin, false);
    low.options.insert("reasoning_effort".into(), json!("low"));
    let mut high = low.clone();
    high.options
        .insert("reasoning_effort".into(), json!("high"));
    let first = WerkProvider::connect(low.clone(), Some("first-secret".into()))
        .await
        .unwrap();
    let same = WerkProvider::connect(low, Some("other-secret".into()))
        .await
        .unwrap();
    let changed = WerkProvider::connect(high, None).await.unwrap();
    let request = ModelRequest {
        reasoning_tier: None,
        messages: vec![Message::text(Role::User, "Task")],
        tools: vec![],
        max_output_tokens: 2048,
        output_schema: None,
    };
    assert_eq!(
        first.request_fingerprint(&request),
        same.request_fingerprint(&request)
    );
    assert_ne!(
        first.request_fingerprint(&request),
        changed.request_fingerprint(&request)
    );
}

#[tokio::test]
async fn protocol_reasoning_tiers_are_opaque_and_defaults_reach_count_and_generation() {
    for tier in [None, Some("default"), Some("ultra")] {
        let server=Server::start(|request| match request.path.as_str() {
            "/werk/v1/capabilities" => control(json!({"capabilities":[
                {"id":"api.chat.reasoning","status":"supported","detail":"fixture","operations":["profile","tiers"]},
                {"id":"api.chat.token_count","status":"supported","detail":"fixture","operations":["count_tokens"]}]})),
            "/werk/v1/chat/count_tokens" => control(json!({"model":"fixture-model","input_tokens":32})),
            path if path.starts_with("/werk/v1/chat/reasoning?") => {
                let url=reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
                let query=url.query_pairs().collect::<std::collections::BTreeMap<_,_>>();
                let output=query["max_output_tokens"].parse::<u64>().unwrap();
                control(json!({"schema_version":1,"model":query["model"],"supported":true,
                    "default_tier":if output>=2048 {"ultra"} else {"low"},"default_adjusted_to_limits":output<2048,
                    "limits":{"context_tokens":32768,"output_tokens":output,"reasoning_tokens":output-512,"answer_reserve_tokens":512},
                    "tiers":[{"id":"low","label":"Low","available":true,"reasoning_tokens":512,"minimum_output_tokens":1024},
                        {"id":"ultra","label":"Ultra","available":output>=2048,"reasoning_tokens":1536,"minimum_output_tokens":2048}]}))
            },
            _ => werk(request),
        }).await;
        let mut c = config(&server.origin, false);
        if let Some(tier) = tier {
            c.options.insert("reasoning_tier".into(), json!(tier));
        }
        let provider = WerkProvider::connect(c, None).await.unwrap();
        let large = provider
            .reasoning_profile("fixture-model", 2048)
            .await
            .unwrap()
            .unwrap();
        let small = provider
            .reasoning_profile("fixture-model", 1024)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(large.default_tier.as_deref(), Some("ultra"));
        assert_eq!(small.default_tier.as_deref(), Some("low"));
        assert!(!small.tiers[1].available);
        assert!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| r.method == "GET")
        );
        let request = ModelRequest {
            reasoning_tier: None,
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![],
            max_output_tokens: 2048,
            output_schema: None,
        };
        assert_eq!(
            provider.count_input_tokens(&request).await.unwrap(),
            Some(32)
        );
        let mut stream = provider.stream(request).await.unwrap();
        while let Some(delta) = stream.next().await {
            delta.unwrap();
        }
        let records = server.requests.lock().unwrap();
        let sent: Vec<_> = records.iter().filter(|r| r.method == "POST").collect();
        assert_eq!(sent.len(), 2);
        for request in sent {
            assert_eq!(
                request.body["werk"]["reasoning"]["tier"],
                tier.unwrap_or("default")
            );
            assert!(request.body.get("reasoning_effort").is_none());
            assert!(request.body.get("reasoning_budget_tokens").is_none());
            assert_eq!(request.body["max_completion_tokens"], 2048);
        }
    }
}

#[tokio::test]
async fn reasoning_tiers_require_advertised_protocol_and_do_not_replace_native_effort() {
    let server = Server::start(werk).await;
    let mut c = config(&server.origin, false);
    c.options.insert("reasoning_tier".into(), json!("ultra"));
    let error = WerkProvider::connect(c.clone(), None).await.err().unwrap();
    assert_eq!(error.code, "invalid_reasoning_tier");
    c.options.insert("reasoning_effort".into(), json!("high"));
    let before = server.requests.lock().unwrap().len();
    assert_eq!(
        WerkProvider::connect(c, None).await.err().unwrap().code,
        "invalid_reasoning_tier"
    );
    assert_eq!(server.requests.lock().unwrap().len(), before);
    let provider = WerkProvider::connect(config(&server.origin, false), None)
        .await
        .unwrap();
    assert!(
        provider
            .reasoning_profile("fixture-model", 2048)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn request_effort_snapshot_overrides_native_config_and_has_separate_token_counts() {
    let server=Server::start(|request| match request.path.as_str() {
        "/werk/v1/capabilities" => control(json!({"capabilities":[
            {"id":"api.chat.reasoning","status":"supported","detail":"fixture","operations":["profile","tiers"]},
            {"id":"api.chat.token_count","status":"supported","detail":"fixture","operations":["count_tokens"]}]})),
        "/werk/v1/chat/count_tokens" => control(json!({"model":"fixture-model","input_tokens":match request.body["werk"]["reasoning"]["tier"].as_str(){Some("custom-v2")=>42,Some("default")=>64,_=>88}})),
        _=>werk(request),
    }).await;
    let mut config = config(&server.origin, false);
    config
        .options
        .insert("reasoning_effort".into(), json!("native"));
    let provider = WerkProvider::connect(config, None).await.unwrap();
    let mut fingerprints = std::collections::BTreeSet::new();
    for (tier, tokens) in [(None, 88), (Some("custom-v2"), 42), (Some("default"), 64)] {
        let request = ModelRequest {
            reasoning_tier: tier.map(str::to_owned),
            messages: vec![Message::text(Role::User, "same input")],
            tools: vec![],
            max_output_tokens: 4096,
            output_schema: None,
        };
        fingerprints.insert(provider.request_fingerprint(&request));
        assert_eq!(
            provider.count_input_tokens(&request).await.unwrap(),
            Some(tokens)
        );
        let mut stream = provider.stream(request).await.unwrap();
        while let Some(delta) = stream.next().await {
            delta.unwrap();
        }
    }
    assert_eq!(fingerprints.len(), 3);
    let records = server.requests.lock().unwrap();
    let posts: Vec<_> = records.iter().filter(|r| r.method == "POST").collect();
    assert_eq!(posts.len(), 6);
    for (pair, tier) in posts
        .chunks(2)
        .zip([None, Some("custom-v2"), Some("default")])
    {
        assert!(pair[0].path.ends_with("count_tokens"));
        for request in pair {
            if let Some(tier) = tier {
                assert_eq!(request.body["werk"]["reasoning"]["tier"], tier);
                assert!(request.body.get("reasoning_effort").is_none());
            } else {
                assert_eq!(request.body["reasoning_effort"], "native");
            }
        }
    }
}

#[tokio::test]
async fn reasoning_effort_reaches_token_count_and_both_chat_transports() {
    for streaming in [false, true] {
        for effort in [
            None,
            Some(Value::Null),
            Some(json!("none")),
            Some(json!("minimal")),
            Some(json!("low")),
            Some(json!("medium")),
            Some(json!("high")),
            Some(json!("xhigh")),
            Some(json!("max")),
            Some(json!("native-level")),
            Some(json!(0.75)),
        ] {
            let server = Server::start(|request| match request.path.as_str() {
                "/werk/v1/capabilities" => control(json!({"capabilities":[{"id":"api.chat.token_count","status":"supported","detail":"fixture","operations":["count_tokens"]}]})),
                "/werk/v1/chat/count_tokens" => control(json!({"model":"fixture-model","input_tokens":32})),
                "/v1/chat/completions" if request.body["stream"] == true => Response {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    body: b"data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_vec(),
                },
                _ => werk(request),
            }).await;
            let mut configuration = config(&server.origin, streaming);
            if let Some(value) = &effort {
                configuration
                    .options
                    .insert("reasoning_effort".into(), value.clone());
            }
            let provider = WerkProvider::connect(configuration, None).await.unwrap();
            let request = ModelRequest {
                reasoning_tier: None,
                messages: vec![Message::text(Role::User, "Task")],
                tools: vec![ToolDefinition {
                    name: "files.read".into(),
                    version: "1".into(),
                    description: "Read a file".into(),
                    input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
                    effect: Effect::Read,
                    timeout_ms: 1000,
                    output_limit: 1024,
                    concurrency_safe: true,
                }],
                max_output_tokens: 1024,
                output_schema: None,
            };
            assert_eq!(
                provider.count_input_tokens(&request).await.unwrap(),
                Some(32)
            );
            let mut stream = provider.stream(request).await.unwrap();
            let mut assembler = ResponseAssembler::default();
            while let Some(delta) = stream.next().await {
                assembler.push(&delta.unwrap(), 65536).unwrap();
            }
            assert_eq!(assembler.finish().unwrap().0.content, "done");
            let records = server.requests.lock().unwrap();
            let bodies: Vec<_> = records.iter().filter(|r| r.method == "POST").collect();
            assert_eq!(bodies.len(), 2, "counts are cached; no extra inference");
            for request in bodies {
                assert_eq!(
                    request.body.get("reasoning_effort"),
                    effort.as_ref().filter(|v| !v.is_null())
                );
                assert_eq!(request.body["max_completion_tokens"], 1024);
                assert_eq!(request.body["tools"][0]["function"]["name"], "files.read");
                assert!(request.body.get("reasoning_budget").is_none());
            }
        }
    }
}

#[tokio::test]
async fn invalid_reasoning_types_and_unlisted_options_fail_before_network() {
    let server = Server::start(werk).await;
    for (key, value, code) in [
        ("reasoning_effort", json!(true), "invalid_reasoning_effort"),
        ("reasoning_effort", json!([]), "invalid_reasoning_effort"),
        (
            "reasoning_effort",
            json!({"level":"high"}),
            "invalid_reasoning_effort",
        ),
        ("reasoning_budget", json!(2048), "unsupported_option"),
        ("messages", json!([]), "unsupported_option"),
    ] {
        let mut configuration = config(&server.origin, false);
        configuration.options.insert(key.into(), value);
        assert_eq!(
            WerkProvider::connect(configuration, None)
                .await
                .err()
                .unwrap()
                .code,
            code
        );
    }
    assert!(server.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn protocol_reasoning_defaults_are_model_scoped_fresh_and_never_frozen_as_overrides() {
    for case in [
        "reported",
        "wrong-model",
        "stale",
        "busy",
        "disagree",
        "missing",
        "malformed",
    ] {
        let server = Server::start(move |request| {
            if request.path != "/werk/v1/observability" { return werk(request); }
            let mut worker = json!({"model":"fixture-model","backend":"llama.cpp / CUDA","available":true,
                "reasoning_defaults":{"source":"worker_launch_configuration","effort":"high","thinking":false,"budget_tokens":2048},
                "gauges":{"slots_total":1,"context_per_request_tokens":32768}});
            match case {
                "wrong-model" => worker["model"] = json!("another-model"),
                "busy" => worker["available"] = json!(false),
                "missing" => { worker.as_object_mut().unwrap().remove("reasoning_defaults"); },
                "malformed" => worker["reasoning_defaults"]["effort"] = json!(true),
                _ => {}
            }
            let mut backends = vec![worker.clone()];
            if case == "disagree" {
                worker["reasoning_defaults"]["effort"] = json!("low");
                backends.push(worker);
            }
            control(json!({"schema_version":1,"observed_at_ms":20000,"backend_observed_at_ms":if case == "stale" {1} else {20000},"backends":backends}))
        }).await;
        let provider = WerkProvider::connect(config(&server.origin, false), None)
            .await
            .unwrap();
        if case == "reported" {
            let defaults = &provider.reasoning_defaults["fixture-model"];
            assert_eq!(defaults.effort, Some(json!("high")));
            assert_eq!(defaults.thinking, Some(false));
            assert_eq!(defaults.budget_tokens, Some(2048));
        } else {
            assert!(
                !provider.reasoning_defaults.contains_key("fixture-model"),
                "{case}"
            );
        }
        let mut stream = provider
            .stream(ModelRequest {
                reasoning_tier: None,
                messages: vec![Message::text(Role::User, "Task")],
                tools: vec![],
                max_output_tokens: 1024,
                output_schema: None,
            })
            .await
            .unwrap();
        while let Some(delta) = stream.next().await {
            delta.unwrap();
        }
        let requests = server.requests.lock().unwrap();
        let sent = &requests
            .iter()
            .find(|r| r.path == "/v1/chat/completions")
            .unwrap()
            .body;
        // Echoing high would force thinking on, overriding the server's false.
        // Inherit the native defaults as a unit instead of reconstructing them.
        assert!(sent.get("reasoning_effort").is_none());
        assert!(sent.get("reasoning_budget").is_none());
        assert_eq!(sent["max_completion_tokens"], 1024);
    }
}

#[tokio::test]
async fn rejected_reasoning_keeps_server_explanation_without_credentials_or_retry() {
    let server = Server::start(|request| {
        if request.path != "/v1/chat/completions" {
            return werk(request);
        }
        let mut response = Response::json(json!({"error":{
            "code":"invalid_reasoning_effort", "param":"reasoning_effort",
            "message":"Supported values: none, high (fixture-secret)"
        }}));
        response.status = 400;
        response
    })
    .await;
    for streaming in [false, true] {
        let mut configuration = config(&server.origin, streaming);
        configuration
            .options
            .insert("reasoning_effort".into(), json!("max"));
        let provider = WerkProvider::connect(configuration, Some("fixture-secret".into()))
            .await
            .unwrap();
        let error = provider
            .stream(ModelRequest {
                reasoning_tier: None,
                messages: vec![Message::text(Role::User, "Task")],
                tools: vec![],
                max_output_tokens: 1024,
                output_schema: None,
            })
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "invalid_reasoning_effort");
        assert!(error.message.contains("Supported values: none, high"));
        assert!(!error.message.contains("fixture-secret"));
        assert!(!error.retryable);
    }
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "POST")
            .count(),
        2
    );
}

#[tokio::test]
async fn streaming_timeout_measures_inactivity_not_total_generation_time() {
    use std::time::Duration;
    for (streaming, pause, expected_error) in [
        (true, 30, None),
        (true, 600, Some("timeout")),
        (false, 30, Some("network_error")),
    ] {
        let server = Server::with_body_delay(
            move |request| {
                if request.path != "/v1/chat/completions" {
                    return werk(request);
                }
                if !streaming {
                    return Response::json(json!({"choices":[{"message":{"content":"x".repeat(1000)},"finish_reason":"stop"}]}));
                }
                let mut body = String::new();
                for _ in 0..10 {
                    body.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n");
                }
                body.push_str("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n");
                Response {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    body: body.into_bytes(),
                }
            },
            Some((24, Duration::from_millis(pause))),
        ).await;
        let mut configuration = config(&server.origin, streaming);
        configuration.timeout_ms = 300;
        let provider = WerkProvider::connect(configuration, None).await.unwrap();
        let started = std::time::Instant::now();
        let result = async {
            let mut stream = provider
                .stream(ModelRequest {
                    reasoning_tier: None,
                    messages: vec![Message::text(Role::User, "generate")],
                    tools: vec![],
                    max_output_tokens: 1024,
                    output_schema: None,
                })
                .await?;
            let mut assembler = ResponseAssembler::default();
            while let Some(delta) = stream.next().await {
                assembler.push(&delta?, 65536)?;
            }
            assembler.finish()
        }
        .await;
        if let Some(code) = expected_error {
            assert_eq!(result.unwrap_err().code, code);
        } else {
            assert_eq!(result.unwrap().0.content, "hello".repeat(10));
            assert!(started.elapsed() > Duration::from_millis(600));
        }
    }
}

#[tokio::test]
async fn timed_out_inference_is_not_retried_and_shared_connection_blocks_followups() {
    use std::time::Duration;
    for (streaming, before_headers) in [(true, false), (true, true), (false, false)] {
        let server = Server::with_chat_delays(
            werk,
            before_headers.then_some(Duration::from_secs(2)),
            (!before_headers).then_some((16, Duration::from_secs(2))),
        )
        .await;
        let mut configuration = config(&server.origin, streaming);
        configuration.timeout_ms = 150;
        let provider = Arc::new(WerkProvider::connect(configuration, None).await.unwrap());
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
                AgentProfile {
                    schema_version: 1,
                    name: "timeout-fixture".into(),
                    prompt: "Do the task".into(),
                    tools: vec![],
                    skills: vec![],
                },
                dir.path(),
                HostPolicy::default(),
                Value::Null,
            )
            .await
            .unwrap();
        let limits = Limits {
            max_retries: 2,
            max_wall_ms: 5000,
            ..Default::default()
        };
        let run = session
            .submit("Task", "start", limits.clone(), None)
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.status, RunStatus::Failed);
        let error = result.error.unwrap();
        assert!(matches!(error.code.as_str(), "timeout" | "network_error"));
        assert!(!error.retryable);
        assert_eq!(result.metrics.model_requests, 1);
        assert_eq!(result.metrics.tool_invocations, 0);
        let before = server.requests.lock().unwrap().len();
        let followup = session
            .submit("Continue", "followup", limits, None)
            .await
            .unwrap();
        assert_eq!(
            followup.wait().await.unwrap().error.unwrap().code,
            "inference_unresolved"
        );
        assert_eq!(server.requests.lock().unwrap().len(), before);
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
        let data = session.snapshot().await.unwrap();
        assert_eq!(data.runs[0].limits.max_retries, 2);
        assert_eq!(data.runs[0].attempts.len(), 1);
        let other_dir = tempfile::tempdir().unwrap();
        let other_session = engine
            .create_session(
                data.profile.clone(),
                other_dir.path(),
                HostPolicy::default(),
                Value::Null,
            )
            .await
            .unwrap();
        let other_run = other_session
            .submit("Task", "other-session", Limits::default(), None)
            .await
            .unwrap();
        assert_eq!(
            other_run.wait().await.unwrap().error.unwrap().code,
            "inference_unresolved"
        );
        assert_eq!(server.requests.lock().unwrap().len(), before);
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn abandoning_an_unpolled_inference_stream_blocks_new_dispatch() {
    let server = Server::start(werk).await;
    let provider = WerkProvider::connect(config(&server.origin, true), None)
        .await
        .unwrap();
    let request = ModelRequest {
        reasoning_tier: None,
        messages: vec![Message::text(Role::User, "Task")],
        tools: vec![],
        max_output_tokens: 1024,
        output_schema: None,
    };
    let stream = provider.stream(request.clone()).await.unwrap();
    drop(stream);
    let before = server.requests.lock().unwrap().len();
    assert_eq!(
        provider.stream(request).await.err().unwrap().code,
        "inference_unresolved"
    );
    assert_eq!(server.requests.lock().unwrap().len(), before);
}

#[tokio::test]
async fn cancelling_before_response_headers_blocks_inference_and_token_counting() {
    let received = Arc::new(tokio::sync::Notify::new());
    let server = Server::with_chat_delays(
        {
            let received = received.clone();
            move |request| {
                if request.path == "/v1/chat/completions" {
                    received.notify_one();
                }
                werk(request)
            }
        },
        Some(std::time::Duration::from_secs(2)),
        None,
    )
    .await;
    let provider = Arc::new(
        WerkProvider::connect(config(&server.origin, true), None)
            .await
            .unwrap(),
    );
    let request = ModelRequest {
        reasoning_tier: None,
        messages: vec![Message::text(Role::User, "Task")],
        tools: vec![],
        max_output_tokens: 1024,
        output_schema: None,
    };
    let task = tokio::spawn({
        let provider = provider.clone();
        let request = request.clone();
        async move { provider.stream(request).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), received.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.err().unwrap().is_cancelled());
    let before = server.requests.lock().unwrap().len();
    assert_eq!(
        provider
            .count_input_tokens(&request)
            .await
            .unwrap_err()
            .code,
        "inference_unresolved"
    );
    assert_eq!(
        provider.stream(request).await.err().unwrap().code,
        "inference_unresolved"
    );
    assert_eq!(server.requests.lock().unwrap().len(), before);
}

#[tokio::test]
async fn incomplete_stream_blocks_followups_but_reported_output_limit_does_not() {
    for finished in [false, true] {
        let server = Server::start(move |request| {
            if request.path != "/v1/chat/completions" {
                return werk(request);
            }
            let mut body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n".to_owned();
            if finished {
                body.push_str("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n");
            }
            Response {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: body.into_bytes(),
            }
        }).await;
        let provider = WerkProvider::connect(config(&server.origin, true), None)
            .await
            .unwrap();
        let request = ModelRequest {
            reasoning_tier: None,
            messages: vec![Message::text(Role::User, "Task")],
            tools: vec![],
            max_output_tokens: 1024,
            output_schema: None,
        };
        let mut stream = provider.stream(request.clone()).await.unwrap();
        let mut error = None;
        while let Some(delta) = stream.next().await {
            match delta {
                Ok(ModelDelta::Finish { reason }) => {
                    assert_eq!(reason, "length");
                    // Recovery may drop the stream as soon as the limit is reported.
                    break;
                }
                Err(e) => {
                    error = Some(e);
                    break;
                }
                _ => {}
            }
        }
        drop(stream);
        if finished {
            assert!(error.is_none());
            assert!(provider.stream(request).await.is_ok());
        } else {
            let error = error.unwrap();
            assert_eq!(error.code, "interrupted_stream");
            assert!(!error.retryable);
            let before = server.requests.lock().unwrap().len();
            assert_eq!(
                provider.stream(request).await.err().unwrap().code,
                "inference_unresolved"
            );
            assert_eq!(server.requests.lock().unwrap().len(), before);
        }
    }
}

#[tokio::test]
async fn explicit_http_rejection_keeps_bounded_retry_and_completed_connection_usable() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Server::start({
        let calls = calls.clone();
        move |request| {
            if request.path == "/v1/chat/completions" && calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Response {
                    status: 503,
                    headers: vec![],
                    body: b"{}".to_vec(),
                }
            } else {
                werk(request)
            }
        }
    })
    .await;
    let provider = Arc::new(
        WerkProvider::connect(config(&server.origin, false), None)
            .await
            .unwrap(),
    );
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "rejection-fixture".into(),
                prompt: "Do the task".into(),
                tools: vec![],
                skills: vec![],
            },
            dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    for command in ["start", "followup"] {
        let run = session
            .submit("Task", command, Limits::default(), None)
            .await
            .unwrap();
        assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    engine.close().await.unwrap();
}

#[tokio::test]
async fn backend_error_messages_are_preserved_and_credentials_redacted() {
    for streaming in [false, true] {
        let server = Server::start(move |request| {
            if request.path != "/v1/chat/completions" {
                return werk(request);
            }
            let error = json!({"error": {
                "message": "invalid tool-call arguments: test-secret",
                "type": "server_error",
                "code": null
            }});
            if streaming {
                let fragment = json!({"choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":0,"id":"partial",
                    "function":{"name":"files.write","arguments":"{"}
                }]}}]});
                Response {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    body: format!("data: {fragment}\n\ndata: {error}\n\n").into_bytes(),
                }
            } else {
                Response::json(error)
            }
        })
        .await;
        let provider = WerkProvider::connect(
            config(&server.origin, streaming),
            Some("test-secret".into()),
        )
        .await
        .unwrap();
        let response = provider
            .stream(ModelRequest {
                reasoning_tier: None,
                messages: vec![Message::text(Role::User, "write a file")],
                tools: vec![],
                max_output_tokens: 64,
                output_schema: None,
            })
            .await;
        let error = match response {
            Err(error) => error,
            Ok(mut stream) => loop {
                match stream
                    .next()
                    .await
                    .expect("stream must report the backend failure")
                {
                    Err(error) => break error,
                    Ok(ModelDelta::Done | ModelDelta::Finish { .. }) => {
                        panic!("failed tool call must not complete")
                    }
                    Ok(_) => {}
                }
            },
        };
        assert_eq!(error.code, "provider_error");
        assert_eq!(error.message, "invalid tool-call arguments: [redacted]");
        assert!(!error.retryable);
    }
}
#[tokio::test]
async fn discovery_accepts_capabilities_without_operations() {
    let server = Server::start(|request| {
        if request.path == "/werk/v1/capabilities" {
            control(json!({"capabilities":[
                {"id":"runtime.state.persistence","status":"unsupported","detail":"not exposed by backend"},
                {"id":"runtime.memory.reservations","status":"unavailable","detail":"no bounded estimates"},
                {"id":"runtime.memory.telemetry.host","status":"supported","detail":"sampled dynamically","operations":["read"]}
            ]}))
        } else {
            werk(request)
        }
    }).await;
    let provider = WerkProvider::connect(config(&server.origin, false), None)
        .await
        .unwrap();
    assert_eq!(provider.runtime_capabilities.len(), 3);
    for capability in &provider.runtime_capabilities[..2] {
        assert!(capability.operations.is_empty());
        assert!(!capability.status.executable(true));
    }
    assert_eq!(provider.runtime_capabilities[2].operations, ["read"]);
    assert_eq!(
        provider.discovery["models"]["data"][0]["id"],
        "fixture-model"
    );
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.method == "GET")
    );
}

#[tokio::test]
async fn chat_connect_does_not_depend_on_the_full_task_catalogue() {
    let server = Server::start(|request| {
        if request.path == "/v1/capabilities" || request.path.starts_with("/v1/parameters") {
            return Response {
                status: 503,
                headers: vec![],
                body: b"catalogue probe unavailable".to_vec(),
            };
        }
        werk(request)
    })
    .await;
    let provider = WerkProvider::connect(config(&server.origin, false), None)
        .await
        .unwrap();
    assert_eq!(
        provider.discovery["models"]["data"][0]["id"],
        "fixture-model"
    );
    assert!(!provider.runtime_capabilities.is_empty());
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| { r.path != "/v1/capabilities" && !r.path.starts_with("/v1/parameters") }),
        "Connecting must not start a global backend probe, even in the background"
    );
}

#[tokio::test]
async fn native_discovery_then_chat_and_correct_tool_pairs() {
    let server=Server::start(|request|{
        if request.path=="/v1/chat/completions"&&!request.body["messages"].as_array().unwrap().iter().any(|m|m["role"]=="tool"){
            Response::json(json!({"choices":[{"message":{"content":"Planning","tool_calls":[{"id":"c1","type":"function","function":{"name":"task.plan","arguments":"{\"steps\":[]}"}}]},"finish_reason":"tool_calls"}]}))
        }else{werk(request)}
    }).await;
    let provider = WerkProvider::connect(config(&server.origin, false), Some("test-secret".into()))
        .await
        .unwrap();
    assert_eq!(
        provider.runtime_capabilities[0].status,
        CapabilityStatus::ExternallyManaged
    );
    let engine = WorkerEngine::builder(Arc::new(provider))
        .harness(werk_worker::harness::HarnessConfig {
            coding_checks: false,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let profile = AgentProfile {
        schema_version: 1,
        name: "test".into(),
        prompt: "test".into(),
        tools: vec!["task.plan".into()],
        skills: vec![],
    };
    let session = engine
        .create_session(profile, dir.path(), HostPolicy::default(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("plan", "c", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let requests = server.requests.lock().unwrap().clone();
    assert_eq!(requests[0].path, "/werk/v1/info");
    assert!(
        requests[1..3]
            .iter()
            .any(|r| r.path == "/werk/v1/capabilities")
    );
    assert!(requests[1..3].iter().any(|r| r.path == "/v1/models"));
    let chats: Vec<_> = requests
        .iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .collect();
    assert_eq!(chats.len(), 2);
    let messages = chats[1].body["messages"].as_array().unwrap();
    let tool = messages.iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(tool["tool_call_id"], "c1");
    assert!(!requests.iter().any(|r| r.path.contains("prefill")));
    assert!(
        !session
            .export()
            .await
            .unwrap()
            .to_string()
            .contains("test-secret")
    );
    engine.close().await.unwrap();
}
#[tokio::test]
async fn envelope_version_header_request_id_redirect_auth_and_limits() {
    for kind in [
        "version",
        "header",
        "request_id",
        "redirect",
        "auth",
        "limit",
        "retry",
    ] {
        let server=Server::start(move |_|{
            let mut response=control(json!({}));
            match kind{
                "version"=>response.body=json!({"protocol":{"major":2,"minor":0},"request_id":"r","data":{}}).to_string().into_bytes(),
                "header"=>response.headers[1].1="2.0".into(),
                "request_id"=>response.headers[2].1="different".into(),
                "redirect"=>{response.status=302;response.headers.push(("location".into(),"http://127.0.0.1:1/steal".into()));},
                "auth"|"retry"=>{response.status=if kind=="auth"{401}else{503};response.body=json!({"protocol":{"major":1,"minor":0},"request_id":"req_test","error":{"code":"backend","message":"test-secret","retryable":true}}).to_string().into_bytes();},
                "limit"=>response.body=vec![b'x';4096],_=>{},
            }response
        }).await;
        let mut cfg = config(&server.origin, false);
        cfg.max_body_bytes = 2048;
        let client = WerkClient::new(&cfg, Some("test-secret".into())).unwrap();
        let error = client.control("/werk/v1/info").await.unwrap_err();
        assert!(!error.to_string().contains("test-secret"));
        assert_eq!(error.retryable, kind == "retry", "{kind}: {error}");
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}
#[tokio::test]
async fn streaming_wire_usage_and_native_allowlist_are_separate_from_grants() {
    let server=Server::start(|request|{if request.path=="/v1/chat/completions"{Response{status:200,headers:vec![("content-type".into(),"text/event-stream".into())],body:b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n".to_vec()}}else{werk(request)}}).await;
    let provider = WerkProvider::connect(config(&server.origin, true), None)
        .await
        .unwrap();
    let tools = WerkNativeTool::discover(
        provider.client.clone(),
        &["image_generation".into()],
        &provider.runtime_capabilities,
        false,
    )
    .await
    .unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].definition().name, "werk.image_generation");
    assert!(
        WerkNativeTool::discover(
            provider.client.clone(),
            &["admin_prune".into()],
            &provider.runtime_capabilities,
            false
        )
        .await
        .is_err()
    );
    let mut stream = provider
        .stream(ModelRequest {
            reasoning_tier: None,
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![],
            max_output_tokens: 64,
            output_schema: None,
        })
        .await
        .unwrap();
    let mut assembled = ResponseAssembler::default();
    while let Some(delta) = stream.next().await {
        assembled.push(&delta.unwrap(), 1000).unwrap();
    }
    let (message, usage, _) = assembled.finish().unwrap();
    assert_eq!(message.content, "hello");
    assert_eq!(usage.unwrap().output_tokens, Some(2));
    assert!(!CapabilityStatus::MetadataOnly.executable(true));
    assert!(!CapabilityStatus::ExternallyManaged.executable(true));
    assert!(!CapabilityStatus::Experimental.executable(false));
    assert!(CapabilityStatus::Experimental.executable(true));
}
#[tokio::test]
async fn generic_fetch_blocks_private_targets_and_redirects() {
    let server = Server::start(|_| Response {
        status: 302,
        headers: vec![(
            "location".into(),
            "http://169.254.169.254/latest/meta-data".into(),
        )],
        body: vec![],
    })
    .await;
    assert_eq!(
        runtime::fetch(&server.origin, "GET", None)
            .await
            .unwrap_err()
            .code,
        "network_denied"
    );
    assert!(server.requests.lock().unwrap().is_empty());
    assert_eq!(
        runtime::fetch(&server.origin, "GET", Some(&server.origin))
            .await
            .unwrap_err()
            .code,
        "redirect_denied"
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn concurrency_discovery_matches_alias_and_falls_back_gracefully() {
    for (case, expected) in [
        ("verified", Some(5)),
        ("missing", None),
        ("denied", None),
        ("malformed", None),
    ] {
        let server=Server::start(move |request| {
            if request.path == "/werk/v1/deployments" {
                match case {
                    "verified" => Response::json(json!({"deployments":{"profiles":[
                        {"plan":{"profile":{"id":"a","alias":"fixture-model","parallel":2}},"available_slots":0},
                        {"plan":{"profile":{"id":"b","alias":"fixture-model","parallel":3}},"available_slots":1},
                        {"plan":{"profile":{"id":"c","alias":"different","parallel":50}}}
                    ]}})),
                    "denied" => Response{status:403,headers:vec![],body:b"{}".to_vec()},
                    "malformed" => Response::json(json!({"deployments":{"profiles":[{"plan":{"profile":{"id":"a","alias":"fixture-model","parallel":0}}}]}})),
                    _ => werk(request),
                }
            } else {werk(request)}
        }).await;
        let provider = WerkProvider::connect(config(&server.origin, false), None)
            .await
            .unwrap();
        assert_eq!(provider.concurrency().max_requests, expected, "{case}");
        let mut stream = provider
            .stream(ModelRequest {
                reasoning_tier: None,
                messages: vec![Message::text(Role::User, "hi")],
                tools: vec![],
                max_output_tokens: 64,
                output_schema: None,
            })
            .await
            .unwrap();
        while let Some(delta) = stream.next().await {
            delta.unwrap();
        }
    }
}

#[tokio::test]
async fn protocol_observability_accepts_fresh_slots_not_stale_or_other_models() {
    for (case, expected) in [
        ("fresh", Some(4)),
        ("stale", None),
        ("other", None),
        ("fractional", None),
    ] {
        let server=Server::start(move |request| {
            if request.path == "/werk/v1/observability" {
                control(json!({"schema_version":1,"observed_at_ms":20_000,"backend_observed_at_ms":if case=="stale" {1} else {20_000},"backends":[{"model":if case=="other" {"unrelated"} else {"fixture-model"},"backend":"llama.cpp / CPU","available":true,"gauges":{"slots_total":if case=="fractional" {4.5} else {4.0}}}]}))
            } else {werk(request)}
        }).await;
        let provider = WerkProvider::connect(config(&server.origin, false), None)
            .await
            .unwrap();
        assert_eq!(provider.concurrency().max_requests, expected, "{case}");
        if expected.is_some() {
            assert_eq!(provider.concurrency().source, "werk_protocol_observability");
        }
    }
}

#[tokio::test]
async fn server_context_discovery_uses_per_request_minimum_and_requires_evidence() {
    for (runtime, expected) in [("llama_cuda", 8192), ("vllm_cuda", 8192)] {
        let server = Server::start(move |r| {
            if r.path == "/werk/v1/deployments" {
                Response::json(json!({"deployments":{"profiles":[{"plan":{"profile":{"id":"a","alias":"fixture-model","parallel":4,"context":16384,"runtime":runtime}}},{"plan":{"profile":{"id":"b","alias":"fixture-model","parallel":2,"context":8192,"runtime":runtime}}}]}}))
            } else { werk(r) }
        }).await;
        let provider = WerkProvider::connect(config(&server.origin, false), None)
            .await
            .unwrap();
        assert_eq!(provider.capabilities().context_tokens, Some(expected));
    }
    let server = Server::start(werk).await;
    let mut cfg = config(&server.origin, false);
    cfg.capabilities.context_tokens = None;
    let provider = WerkProvider::connect(cfg, None).await.unwrap();
    let result = provider
        .stream(ModelRequest {
            reasoning_tier: None,
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![],
            max_output_tokens: 64,
            output_schema: None,
        })
        .await;
    assert_eq!(result.err().unwrap().code, "context_limit_unknown");
    assert!(
        !server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path == "/v1/chat/completions")
    );
}

#[tokio::test]
async fn protocol_slot_minimum_clamps_host_without_guessing_from_aggregate() {
    for (minimum, expected) in [
        (json!(8192), Some(8192)),
        (Value::Null, None),
        (json!(0), None),
        (json!(8192.5), None),
    ] {
        let server = Server::start(move |r| {
            if r.path == "/werk/v1/observability" {
                let mut gauges = json!({"slots_total":2,"context_capacity_tokens":40960});
                if !minimum.is_null() { gauges["context_per_request_tokens"] = minimum.clone(); }
                control(json!({"schema_version":1,"observed_at_ms":now_ms(),"backend_observed_at_ms":now_ms(),
                    "backends":[{"model":"fixture-model","backend":"llama.cpp / CUDA","available":true,"gauges":gauges}]}))
            } else { werk(r) }
        }).await;
        let mut cfg = config(&server.origin, false);
        cfg.capabilities.context_tokens = None;
        let provider = WerkProvider::connect(cfg, None).await.unwrap();
        assert_eq!(provider.capabilities().context_tokens, expected);
        if expected.is_some() {
            assert_eq!(
                provider.context_limit_source,
                "werk_protocol_and_host_minimum"
            );
            assert!(
                !server
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.path == "/werk/v1/deployments")
            );
        }
    }
}

#[tokio::test]
async fn protocol_native_count_keeps_large_current_file_work_in_context() {
    let source = "<script>/*ü*/</script>".repeat(700);
    let expected = source.clone();
    let server = Server::start(move |r| {
        match r.path.as_str() {
            "/werk/v1/capabilities" => control(json!({"capabilities":[{"id":"api.chat.token_count","status":"supported","detail":"fixture native counter","operations":["count_tokens"]}]})),
            "/werk/v1/chat/count_tokens" => {
                assert!(r.raw.contains("x-werk-protocol-version: 1.0"));
                assert_eq!(r.body["stream"], false);
                assert_eq!(r.body["messages"][1]["content"], expected);
                control(json!({"model":"fixture-model","input_tokens":1500}))
            },
            "/v1/chat/completions" => {
                assert_eq!(r.body["messages"][1]["content"], expected);
                werk(r)
            },
            _ => werk(r),
        }
    }).await;
    let mut configuration = config(&server.origin, false);
    configuration.capabilities.context_tokens = Some(4096);
    let provider = Arc::new(WerkProvider::connect(configuration, None).await.unwrap());
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
            AgentProfile {
                schema_version: 1,
                name: "test".into(),
                prompt: "Keep the complete user source".into(),
                tools: vec![],
                skills: vec![],
            },
            dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            source.clone(),
            "start",
            Limits {
                context_tokens: 4096,
                reserve_output_tokens: 1024,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(
        run.status().await.unwrap().metrics.estimated_input_tokens,
        1500
    );
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == "/werk/v1/chat/count_tokens")
            .count(),
        1,
        "identical request counts must be cached"
    );
    let original = ModelRequest {
        reasoning_tier: None,
        messages: vec![
            Message::text(Role::System, "Keep the complete user source"),
            Message::text(Role::User, source),
        ],
        tools: vec![],
        max_output_tokens: 1024,
        output_schema: None,
    };
    provider.count_input_tokens(&original).await.unwrap();
    assert_eq!(provider.input_token_source(&original), "native_exact");
    for reserve in 512..577 {
        let mut other = original.clone();
        other.max_output_tokens = reserve;
        provider.count_input_tokens(&other).await.unwrap();
    }
    assert_eq!(provider.cached_input_tokens(&original), None);
    assert_eq!(
        provider.input_token_source(&original),
        "provider_unspecified"
    );
    let mut stream = provider.stream(original).await.unwrap();
    let mut assembler = ResponseAssembler::default();
    while let Some(delta) = stream.next().await {
        assembler.push(&delta.unwrap(), 65536).unwrap();
    }
    assembler.finish().unwrap();
    engine.close().await.unwrap();
}

#[tokio::test]
async fn native_counter_respects_protocol_retryability_and_model_identity() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for case in ["transient", "unsupported", "wrong_model"] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let server = Server::start(move |r| {
            if r.path == "/werk/v1/capabilities" {
                return control(json!({"capabilities":[{"id":"api.chat.token_count","status":"supported","detail":"counter","operations":["count_tokens"]}]}));
            }
            if r.path == "/werk/v1/chat/count_tokens" {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if case == "unsupported" || (case == "transient" && n == 0) {
                    let mut response = control(Value::Null);
                    response.status = if case == "unsupported" { 501 } else { 503 };
                    response.body = json!({"protocol":{"major":1,"minor":0},"request_id":"req_test",
                        "error":{"code":if case=="unsupported" {"unsupported"} else {"unavailable"},
                        "message":"fixture test-secret","retryable":case=="transient"}}).to_string().into_bytes();
                    return response;
                }
                return control(json!({"model":if case=="wrong_model" {"other"} else {"fixture-model"},"input_tokens":42}));
            }
            werk(r)
        }).await;
        let provider =
            WerkProvider::connect(config(&server.origin, false), Some("test-secret".into()))
                .await
                .unwrap();
        let request = ModelRequest {
            reasoning_tier: None,
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![],
            max_output_tokens: 64,
            output_schema: None,
        };
        let count = provider.count_input_tokens(&request).await;
        match case {
            "transient" => {
                assert_eq!(count.unwrap(), Some(42));
                assert_eq!(
                    provider.count_input_tokens(&request).await.unwrap(),
                    Some(42)
                );
                assert_eq!(calls.load(Ordering::SeqCst), 2);
            }
            "unsupported" => {
                assert_eq!(count.unwrap(), None);
                assert_eq!(provider.count_input_tokens(&request).await.unwrap(), None);
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
            _ => {
                assert_eq!(count.unwrap_err().code, "invalid_envelope");
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
        }
    }
}
#[tokio::test]
async fn smaller_server_context_is_learned_and_rebuilt_without_identical_retry() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let server = Server::start(move |r| {
        if r.path == "/v1/chat/completions" && counter.fetch_add(1,Ordering::SeqCst) == 0 {
            let mut response = Response::json(json!({"error":{"code":"context_length_exceeded","context_length":4096,"message":"private backend message"}}));
            response.status=400; response
        } else {werk(r)}
    }).await;
    let provider = Arc::new(
        WerkProvider::connect(config(&server.origin, false), None)
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
            AgentProfile {
                schema_version: 1,
                name: "test".into(),
                prompt: "test".into(),
                tools: vec![],
                skills: vec![],
            },
            dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("hello", "run", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(provider.capabilities().context_tokens, Some(4096));
    let requests = server.requests.lock().unwrap().clone();
    let chats = requests
        .iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(chats.len(), 2);
    assert_eq!(chats[0].body["max_completion_tokens"], 2048);
    assert_eq!(chats[1].body["max_completion_tokens"], 1024);
    assert!(
        !session
            .export()
            .await
            .unwrap()
            .to_string()
            .contains("private backend message")
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn bare_auth_rejection_is_reported_without_protocol_or_body_leaks() {
    let server = Server::start(|_| Response {
        status: 401,
        headers: vec![],
        body: b"secret supplied by auth middleware".to_vec(),
    })
    .await;
    let client =
        WerkClient::new(&config(&server.origin, false), Some("test-secret".into())).unwrap();
    let error = client.control("/werk/v1/info").await.unwrap_err();
    assert_eq!(error.code, "auth_error");
    assert!(!error.to_string().contains("secret"));
}

#[tokio::test]
async fn image_payload_is_multimodal_and_unsupported_image_count_keeps_text_counter() {
    let server=Server::start(|r| {
        if r.path=="/werk/v1/capabilities" {
            return control(json!({"capabilities":[{"id":"api.chat.token_count","status":"supported","detail":"counter","operations":["count_tokens"]}]}));
        }
        if r.path=="/werk/v1/chat/count_tokens" {
            if r.body["messages"][0]["content"].is_array() {
                let mut response=control(Value::Null);response.status=501;
                response.body=json!({"protocol":{"major":1,"minor":0},"request_id":"req_test","error":{"code":"unsupported","message":"Image counting unavailable","retryable":false}}).to_string().into_bytes();
                return response;
            }
            return control(json!({"model":"fixture-model","input_tokens":42}));
        }
        if r.path=="/v1/chat/completions" {
            let content=&r.body["messages"][0]["content"];
            assert_eq!(content[0]["text"],"Describe this");
            assert!(content[1]["image_url"]["url"].as_str().unwrap().starts_with("data:image/jpeg;base64,"));
        }
        werk(r)
    }).await;
    // Exercise the shipped Qwen preset: its former `unsupported` value blocked
    // image requests before the vision-capable server could receive them.
    let mut configuration: WerkConfig =
        serde_json::from_str(include_str!("../examples/werk-qwen27b.json")).unwrap();
    configuration.base_url = server.origin.clone();
    configuration.model = "fixture-model".into();
    configuration.stream = false;
    assert_eq!(configuration.capabilities.images, Support::Unknown);
    let provider = WerkProvider::connect(configuration, None).await.unwrap();
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 2)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let mut message = Message::text(Role::User, "Describe this");
    message
        .images
        .push(attachments::ImageAttachment::from_bytes(encoded.get_ref()).unwrap());
    let request = ModelRequest {
        reasoning_tier: None,
        messages: vec![message],
        tools: vec![],
        max_output_tokens: 128,
        output_schema: None,
    };
    provider.estimate_input_tokens(&request).await.unwrap();
    assert_eq!(
        provider.input_token_source(&request),
        "native_text_plus_image_estimate"
    );
    let mut stream = provider.stream(request).await.unwrap();
    while let Some(delta) = stream.next().await {
        delta.unwrap();
    }
    assert_eq!(
        provider
            .count_input_tokens(&ModelRequest {
                reasoning_tier: None,
                messages: vec![Message::text(Role::User, "text after image")],
                tools: vec![],
                max_output_tokens: 128,
                output_schema: None
            })
            .await
            .unwrap(),
        Some(42)
    );
}

#[tokio::test]
async fn coding_image_budget_uses_native_text_without_repeating_unsupported_counts() {
    for (native_images, image_count, expected_status) in [
        (None, 1, RunStatus::Completed),
        (None, 2, RunStatus::Failed),
        (Some(900), 1, RunStatus::Completed),
        (Some(16000), 1, RunStatus::Failed),
    ] {
        let server = Server::start(move |r| {
            if r.path == "/werk/v1/capabilities" {
                return control(json!({"capabilities":[{"id":"api.chat.token_count","status":"supported","detail":"counter","operations":["count_tokens"]}]}));
            }
            if r.path == "/werk/v1/chat/count_tokens" {
                let has_images = r.body["messages"].as_array().unwrap().iter().any(|m| m["content"].is_array());
                if has_images && native_images.is_none() {
                    let mut response = control(Value::Null);
                    response.status = 501;
                    response.body = json!({"protocol":{"major":1,"minor":0},"request_id":"req_test","error":{"code":"unsupported","message":"Image counting unavailable","retryable":false}}).to_string().into_bytes();
                    return response;
                }
                assert!(r.body["tools"].as_array().unwrap().len() >= 15);
                return control(json!({"model":"fixture-model","input_tokens":if has_images { native_images.unwrap() } else {2800}}));
            }
            werk(r)
        }).await;
        let mut configuration = config(&server.origin, false);
        configuration.capabilities.context_tokens = Some(16384);
        let provider = Arc::new(WerkProvider::connect(configuration, None).await.unwrap());
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
                AgentProfile::bundled("coding-basic").unwrap(),
                dir.path(),
                HostPolicy {
                    workspace_writable: true,
                    request_approvals: true,
                    ..Default::default()
                },
                Value::Null,
            )
            .await
            .unwrap();
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(677, 680)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let picture = attachments::ImageAttachment::from_bytes(encoded.get_ref()).unwrap();
        let input = "Build a retro Tetris game using the attached reference image.";
        let run = session
            .submit_with_images(
                input,
                vec![picture.clone(); image_count],
                "start",
                Limits {
                    context_tokens: 16384,
                    reserve_output_tokens: 4096,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.status, expected_status, "{:?}", result.error);
        if expected_status == RunStatus::Completed {
            assert_eq!(
                result.metrics.estimated_input_tokens,
                native_images.unwrap_or(2800 + 8192 + 256)
            );
        } else {
            assert_eq!(result.error.unwrap().code, "context_overflow");
        }
        let snapshot = session.snapshot().await.unwrap();
        assert_eq!(snapshot.messages[0].images, vec![picture; image_count]);
        assert_eq!(snapshot.messages[0].content, input);
        let requests = server.requests.lock().unwrap().clone();
        let counts: Vec<_> = requests
            .iter()
            .filter(|r| r.path == "/werk/v1/chat/count_tokens")
            .collect();
        assert_eq!(
            counts
                .iter()
                .filter(|r| r.body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["content"].is_array()))
                .count(),
            1
        );
        assert_eq!(
            counts.len(),
            if native_images.is_some() { 1 } else { 2 },
            "only one unsupported image count and one cached text/tool count"
        );
        let generations: Vec<_> = requests
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .collect();
        assert_eq!(
            generations.len(),
            usize::from(expected_status == RunStatus::Completed)
        );
        if let Some(request) = generations.first() {
            assert_eq!(request.body["messages"][1]["content"][0]["text"], input);
            assert!(
                request.body["messages"][1]["content"][1]["image_url"]["url"]
                    .as_str()
                    .unwrap()
                    .starts_with("data:image/jpeg;base64,")
            );
            assert_eq!(request.body["max_completion_tokens"], 4096);
        }
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn observed_image_overhead_is_bounded_scoped_and_never_an_exact_count() {
    let server = Server::start(|r| {
        if r.path == "/werk/v1/capabilities" {
            return control(json!({"capabilities":[{"id":"api.chat.token_count","status":"supported","detail":"native text counter","operations":["count_tokens"]}]}));
        }
        if r.path == "/werk/v1/chat/count_tokens" {
            if r.body["messages"].as_array().unwrap().iter().any(|m| m["content"].is_array()) {
                let mut response = control(Value::Null);
                response.status = 501;
                response.body = json!({"protocol":{"major":1,"minor":0},"request_id":"req_test","error":{"code":"unsupported","message":"No image counter","retryable":false}}).to_string().into_bytes();
                return response;
            }
            return control(json!({"model":"fixture-model","input_tokens":2800}));
        }
        werk(r)
    }).await;
    let provider = WerkProvider::connect(config(&server.origin, true), None)
        .await
        .unwrap();
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(677, 680)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let picture = attachments::ImageAttachment::from_bytes(encoded.get_ref()).unwrap();
    let mut user = Message::text(Role::User, "Use this reference");
    user.images.push(picture);
    let request = ModelRequest {
        reasoning_tier: None,
        messages: vec![user],
        tools: vec![],
        max_output_tokens: 4096,
        output_schema: None,
    };
    let usage = |input| Usage {
        input_tokens: input,
        output_tokens: Some(30),
        cached_tokens: Some(3200),
    };
    provider.observe_completed_usage(&request, &usage(Some(3243)));
    assert_eq!(
        provider.estimated_image_tokens(&request),
        None,
        "no native baseline yet"
    );
    assert_eq!(
        provider.estimate_input_tokens(&request).await.unwrap(),
        11248
    );
    for input in [None, Some(0), Some(2700), Some(u64::MAX)] {
        provider.observe_completed_usage(&request, &usage(input));
        assert_eq!(provider.estimated_image_tokens(&request), None);
    }
    provider.observe_completed_usage(&request, &usage(Some(3243)));
    assert_eq!(
        provider.estimate_input_tokens(&request).await.unwrap(),
        3755
    );
    assert_eq!(provider.count_input_tokens(&request).await.unwrap(), None);
    assert_eq!(provider.cached_input_tokens(&request), None);
    // New text shares only the image reserve; its native count is still taken.
    let mut next = request.clone();
    next.messages
        .push(Message::text(Role::Assistant, "Continue implementing."));
    assert_eq!(provider.estimate_input_tokens(&next).await.unwrap(), 3755);
    let mut moved = request.clone();
    moved
        .messages
        .insert(0, Message::text(Role::System, "Different placement"));
    assert_eq!(provider.estimated_image_tokens(&moved), None);
    let mut doubled = request.clone();
    doubled.messages[0]
        .images
        .push(request.messages[0].images[0].clone());
    assert_eq!(provider.estimated_image_tokens(&doubled), None);
    let mut other = request.clone();
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        677,
        680,
        image::Rgb([255, 0, 0]),
    ))
    .write_to(&mut encoded, image::ImageFormat::Png)
    .unwrap();
    other.messages[0].images[0] =
        attachments::ImageAttachment::from_bytes(encoded.get_ref()).unwrap();
    assert_eq!(
        provider.estimated_image_tokens(&other),
        None,
        "same dimensions are insufficient"
    );
    provider.observe_completed_usage(&request, &usage(Some(6000)));
    provider.observe_completed_usage(&request, &usage(Some(3243)));
    assert_eq!(
        provider.estimate_input_tokens(&request).await.unwrap(),
        7056,
        "never lower the highest observation"
    );
    let reconnected = WerkProvider::connect(config(&server.origin, true), None)
        .await
        .unwrap();
    assert_eq!(reconnected.estimated_image_tokens(&request), None);
}

#[tokio::test]
async fn image_inspection_keeps_file_hash_missing_file_and_listing_until_edit() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for (streaming, report_usage) in [(false, true), (true, true), (true, false)] {
        let original = "<html>\n<script src=\"main.js\"></script>\n</html>";
        let edited = original.replace("src=\"main.js\"", "type=\"module\"");
        let step = AtomicUsize::new(0);
        let server = Server::start(move |r| {
            if r.path == "/werk/v1/capabilities" {
                return control(json!({"capabilities":[{"id":"api.chat.token_count","status":"supported","detail":"native text counter","operations":["count_tokens"]}]}));
            }
            let text_count = |body: &Value| {
                let mut messages = body["messages"].clone();
                for m in messages.as_array_mut().unwrap() {
                    if m["content"].is_array() { m["content"] = m["content"][0]["text"].clone(); }
                }
                2100 + serde_json::to_vec(&messages).unwrap().len()/4
            };
            if r.path == "/werk/v1/chat/count_tokens" {
                if r.body["messages"].as_array().unwrap().iter().any(|m| m["content"].is_array()) {
                    let mut response = control(Value::Null);
                    response.status = 501;
                    response.body = json!({"protocol":{"major":1,"minor":0},"request_id":"req_test","error":{"code":"unsupported","message":"No image counter","retryable":false}}).to_string().into_bytes();
                    return response;
                }
                return control(json!({"model":"fixture-model","input_tokens":text_count(&r.body)}));
            }
            if r.path != "/v1/chat/completions" { return werk(r); }
            let n = step.fetch_add(1, Ordering::SeqCst);
            if n == 4 {
                let results: Vec<ToolResult> = r.body["messages"].as_array().unwrap().iter()
                    .filter(|m| m["role"] == "tool")
                    .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap()).collect();
                assert!(results.iter().any(|r| r.data["content"] == original && r.data["sha256"] == hash(original)), "read and edit base must survive the missing-file check");
                assert!(results.iter().any(|r| r.error_code.as_deref() == Some("not_found")));
                assert!(results.iter().any(|r| r.data["entries"] == json!(["index.html"])));
                assert!(results.iter().any(|r| r.data["steps"][0]["description"] == "Inspect then implement and verify"));
            }
            let action = match n {
                0 => Some(("task.plan",json!({"steps":[{"id":"1","description":"Inspect then implement and verify","status":"in_progress"}]}))),
                1 => Some(("files.list",json!({"path":"."}))),
                2 => Some(("files.read",json!({"path":"index.html"}))),
                3 => Some(("files.read",json!({"path":"main.js"}))),
                4 => Some(("files.edit",json!({"path":"index.html","old":"src=\"main.js\"","new":"type=\"module\"","expected_hash":hash(original)}))),
                5 => Some(("files.read",json!({"path":"index.html"}))),
                _ => None,
            };
            let finish = if action.is_some() { "tool_calls" } else { "stop" };
            let mut message = json!({"role":"assistant","content":if action.is_some(){""}else{"Verified the changed file."}});
            if let Some((name,args)) = action {
                message["tool_calls"] = json!([{"index":0,"id":format!("call_{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]);
            }
            let usage = json!({"prompt_tokens":text_count(&r.body)+443,"completion_tokens":30,"prompt_tokens_details":{"cached_tokens":3000}});
            if streaming {
                let mut body = format!("data: {}\n\ndata: {}\n\n",json!({"choices":[{"delta":message}]}),json!({"choices":[{"delta":{},"finish_reason":finish}]}));
                if report_usage {body.push_str(&format!("data: {}\n\n",json!({"choices":[],"usage":usage})));}
                body.push_str("data: [DONE]\n\n");
                Response {status:200,headers:vec![("content-type".into(),"text/event-stream".into())],body:body.into_bytes()}
            } else {
                Response::json(json!({"choices":[{"message":message,"finish_reason":finish}],"usage":usage}))
            }
        }).await;
        let mut configuration = config(&server.origin, streaming);
        configuration.capabilities.context_tokens = Some(16384);
        let provider = Arc::new(WerkProvider::connect(configuration, None).await.unwrap());
        let engine = WorkerEngine::builder(provider)
            .harness(werk_worker::harness::HarnessConfig {
                coding_checks: false,
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), original).unwrap();
        let session = engine
            .create_session(
                AgentProfile::bundled("coding-basic").unwrap(),
                dir.path(),
                HostPolicy {
                    workspace_writable: true,
                    request_approvals: true,
                    tool_permissions: ["files.list", "files.read", "files.edit"]
                        .into_iter()
                        .map(|n| (n.to_owned(), policy::ToolPermission::Allow))
                        .collect(),
                    ..Default::default()
                },
                Value::Null,
            )
            .await
            .unwrap();
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(677, 680)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let image = attachments::ImageAttachment::from_bytes(encoded.get_ref()).unwrap();
        let run = session
            .submit_with_images(
                "Complete the game based on this image.",
                vec![image.clone()],
                "start",
                Limits {
                    context_tokens: 16384,
                    reserve_output_tokens: 4096,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(
            result.status,
            RunStatus::Completed,
            "{streaming}/{report_usage}: {:?}",
            result.error
        );
        let state = run.status().await.unwrap();
        assert_eq!(result.metrics.model_requests, 7);
        assert!(
            state.contexts[..5]
                .iter()
                .all(|c| c.omitted_messages.is_empty())
        );
        if report_usage {
            assert!(state.contexts.iter().all(|c| c.omitted_messages.is_empty()));
            assert!(
                state.contexts[1..]
                    .iter()
                    .all(|c| c.estimated_tokens < 6000)
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("index.html")).unwrap(),
            edited
        );
        assert_eq!(
            session.snapshot().await.unwrap().messages[0].images,
            vec![image]
        );
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn image_coding_continues_through_failed_write_edits_and_verification() {
    image_coding_compaction_fixture(false).await;
}

#[tokio::test]
async fn image_coding_compacts_older_turns_before_later_images() {
    image_coding_compaction_fixture(true).await;
}

async fn image_coding_compaction_fixture(prior_turn: bool) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mut contents = "<html><!--next--></html>".to_owned();
    let mut turns = vec![
        (
            "files.write",
            json!({"path":"missing/index.html","content":contents}),
        ),
        (
            "files.write",
            json!({"path":"index.html","content":contents}),
        ),
    ];
    // A single completed edit exceeds available input if its saved body is
    // pinned forever as the latest group. Exercise several such continuations.
    for n in 0..8 {
        let addition = format!("<!-- section {n}: {} -->\n<!--next-->", "code ".repeat(900));
        turns.push(("files.edit", json!({"path":"index.html","old":"<!--next-->","new":addition,"expected_hash":hash(&contents)})));
        contents = contents.replace("<!--next-->", &addition);
    }
    if !prior_turn {
        // Failed proposals must remain intact while older completed groups
        // are compacted; moving the first image would make this one overflow.
        turns.push((
            "files.write",
            json!({"path":"index.html","content":"rejected ".repeat(1400)}),
        ));
    }
    turns.push((
        "files.read",
        json!({"path":"index.html","start_line":9,"end_line":9}),
    ));
    let expected = contents.clone();
    let count = turns.len();
    let step = AtomicUsize::new(0);
    let server = Server::start(move |r| {
        if r.path == "/werk/v1/capabilities" {
            return control(json!({"capabilities":[{"id":"api.chat.token_count","status":"supported","detail":"native text counter","operations":["count_tokens"]}]}));
        }
        let text_count = |body: &Value| {
            let mut messages = body["messages"].clone();
            for message in messages.as_array_mut().unwrap() {
                if message["content"].is_array() {
                    message["content"] = message["content"][0]["text"].clone();
                }
            }
            2100 + serde_json::to_vec(&messages).unwrap().len() / 4
        };
        if r.path == "/werk/v1/chat/count_tokens" {
            let messages = r.body["messages"].as_array().unwrap();
            if messages.iter().any(|m| m["content"].is_array()) {
                let mut response = control(Value::Null);
                response.status = 501;
                response.body = json!({"protocol":{"major":1,"minor":0},"request_id":"req_test","error":{"code":"unsupported","message":"No image counter","retryable":false}}).to_string().into_bytes();
                return response;
            }
            // Deterministic native-counter fixture: templates/tools plus actual
            // text/history length. Images are budgeted by WerkProvider itself.
            let tokens = text_count(&r.body);
            return control(json!({"model":"fixture-model","input_tokens":tokens}));
        }
        if r.path == "/v1/chat/completions" {
            if r.body["messages"].as_array().unwrap().iter().any(|m| m["content"] == "Earlier separate task")
                && !r.body["messages"].as_array().unwrap().iter().any(|m| m["content"].is_array()) {
                return Response::json(json!({"choices":[{"message":{"role":"assistant","content":"Earlier task completed."},"finish_reason":"stop"}]}));
            }
            let n = step.fetch_add(1, Ordering::SeqCst);
            if let Some((name, args)) = turns.get(n) {
                // Learn the small overhead at the original image position.
                // Removing an older turn before a later image changes its
                // placement-specific estimate. Aggregate compaction must still
                // work; an image in the initial user turn should remain stable.
                return Response::json(json!({"choices":[{"message":{"role":"assistant","content":format!("Step {n}: preserve existing functions while implementing the next section."),"tool_calls":[{"id":format!("call_{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":text_count(&r.body)+443,"completion_tokens":100}}));
            }
            return werk(r);
        }
        werk(r)
    }).await;
    let mut configuration = config(&server.origin, false);
    configuration.capabilities.context_tokens = Some(16384);
    let provider = Arc::new(WerkProvider::connect(configuration, None).await.unwrap());
    // Completion gates are tested separately; this fixture isolates image compaction.
    let engine = WorkerEngine::builder(provider)
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
            AgentProfile::bundled("coding-basic").unwrap(),
            dir.path(),
            HostPolicy {
                workspace_writable: true,
                request_approvals: true,
                tool_permissions: ["files.write", "files.edit", "files.read"]
                    .into_iter()
                    .map(|name| (name.to_owned(), policy::ToolPermission::Allow))
                    .collect(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    if prior_turn {
        let earlier = session
            .submit("Earlier separate task", "earlier", Limits::default(), None)
            .await
            .unwrap();
        assert_eq!(earlier.wait().await.unwrap().status, RunStatus::Completed);
    }
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(677, 680)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let picture = attachments::ImageAttachment::from_bytes(encoded.get_ref()).unwrap();
    let input = "Build the game using this reference; preserve all requirements.";
    let run = session
        .submit_with_images(
            input,
            vec![picture.clone()],
            "start",
            Limits {
                context_tokens: 16384,
                reserve_output_tokens: 4096,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(result.metrics.tool_invocations as usize, count);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("index.html")).unwrap(),
        expected
    );
    let saved = session.snapshot().await.unwrap();
    assert_eq!(
        saved
            .messages
            .iter()
            .find(|m| !m.images.is_empty())
            .unwrap()
            .images,
        vec![picture]
    );
    assert_eq!(
        saved
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count(),
        count
    );
    assert!(
        saved
            .messages
            .iter()
            .any(|m| m.content.contains("not_found"))
    );
    assert!(
        run.status()
            .await
            .unwrap()
            .contexts
            .iter()
            .any(|c| !c.omitted_messages.is_empty())
    );
    let state = saved.run(&run.id).unwrap();
    assert!(state.contexts[0].estimated_tokens > 10_000);
    assert!(state.contexts[1].estimated_tokens < 4_000);
    assert!(state.contexts.iter().any(|c| c.omitted_messages.len() >= 4));
    if prior_turn {
        // The fallback for a shifted later image also requires releasing a
        // proven latest write; stable initial images need fewer omissions.
        assert!(state.contexts.iter().enumerate().any(|(step, context)| {
            step.checked_sub(1)
                .and_then(|previous| state.invocations.get(previous))
                .is_some_and(|invocation| {
                    matches!(invocation.call.name.as_str(), "files.write" | "files.edit")
                        && invocation
                            .result
                            .as_ref()
                            .is_some_and(|result| result.status == Outcome::Succeeded)
                        && context.omitted_messages.contains(&invocation.message_index)
                })
        }));
    }
    let requests = server.requests.lock().unwrap().clone();
    let generations: Vec<_> = requests
        .iter()
        .filter(|r| {
            r.path == "/v1/chat/completions"
                && r.body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["content"].is_array())
        })
        .collect();
    assert_eq!(generations.len(), count + 1);
    let positions: Vec<_> = generations
        .iter()
        .map(|request| {
            request.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .position(|message| message["content"].is_array())
                .unwrap()
        })
        .collect();
    if prior_turn {
        assert!(
            positions.iter().any(|index| *index != positions[0]),
            "old turns before the image are compactable"
        );
    } else {
        assert!(
            positions.iter().all(|index| *index == 1),
            "runtime observations must preserve the initial image position"
        );
        let rejected = &state.invocations[count - 2];
        assert_eq!(
            rejected.result.as_ref().unwrap().error_code.as_deref(),
            Some("write_conflict")
        );
        assert!(
            !state.contexts[count - 1]
                .omitted_messages
                .contains(&rejected.message_index)
        );
        let retry_messages = generations[count - 1].body["messages"].as_array().unwrap();
        assert!(
            retry_messages
                .iter()
                .any(|m| m["tool_calls"].as_array().is_some_and(|calls| calls
                    .iter()
                    .any(|call| call["id"] == format!("call_{}", count - 2)))),
            "failed proposal stays available for correction"
        );
    }
    for (n, r) in generations.iter().enumerate() {
        let messages = r.body["messages"].as_array().unwrap();
        if n > 0 {
            assert!(messages.iter().any(|m| m["role"] == "assistant"
                && m["content"]
                    == format!(
                        "Step {}: preserve existing functions while implementing the next section.",
                        n - 1
                    )));
        }
        let user = messages
            .iter()
            .find(|m| m["role"] == "user" && m["content"].is_array())
            .unwrap();
        assert_eq!(user["content"][0]["text"], input);
        assert!(
            user["content"][1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/")
        );
        let budget = r.body["max_completion_tokens"].as_u64().unwrap();
        assert!((3072..=4096).contains(&budget));
        assert!(
            !messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("Output budget:")
        );
    }
    // A read is evidence, not a saved-write body: keep the latest verification.
    let final_messages = generations.last().unwrap().body["messages"]
        .as_array()
        .unwrap();
    assert!(
        final_messages
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"] == format!("call_{}", count - 1))
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path == "/werk/v1/chat/count_tokens"
                && r.body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["content"].is_array()))
            .count(),
        1
    );
    let counts = requests
        .iter()
        .filter(|request| request.path == "/werk/v1/chat/count_tokens")
        .count();
    assert!(
        counts <= generations.len() * 3,
        "compaction must not recount after each optional excerpt line: {counts}"
    );
    println!(
        "IMAGE_COMPACTION_COUNTS prior_turn={prior_turn} count_requests={counts} model_requests={}",
        generations.len()
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn configured_protocol_capacity_survives_missing_observability_and_refreshes() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let slots = Arc::new(AtomicUsize::new(2));
    let reported = slots.clone();
    let server=Server::start(move |request| match request.path.as_str() {
        "/werk/v1/capabilities" => control(json!({"capabilities":[{"id":"api.chat.limits","status":"supported","detail":"fixture","operations":["limits"]}]})),
        path if path.starts_with("/werk/v1/chat/limits?") => control(json!({"schema_version":1,"model":"fixture-model","context_tokens":32768,"max_output_tokens":8192,"max_concurrent_requests":(match reported.load(Ordering::SeqCst) {0 => None,value => Some(value)})})),
        _ => werk(request),
    }).await;
    let provider = WerkProvider::connect(config(&server.origin, false), None)
        .await
        .unwrap();
    assert_eq!(provider.concurrency().max_requests, Some(2));
    assert_eq!(provider.concurrency().source, "werk_protocol_limits");
    slots.store(1, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(5100)).await;
    provider.refresh_concurrency().await;
    assert_eq!(provider.concurrency().max_requests, Some(1));
    for (reported, expected) in [(0, None), (2, Some(2))] {
        slots.store(reported, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(5100)).await;
        provider.refresh_concurrency().await;
        assert_eq!(provider.concurrency().max_requests, expected);
    }
    assert!(
        !server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path == "/v1/chat/completions")
    );
}
