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
    assert_eq!(requests[1].path, "/werk/v1/capabilities");
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
    for (runtime, expected) in [("llama_cuda", 4096), ("vllm_cuda", 8192)] {
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
