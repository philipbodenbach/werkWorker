mod support;

use futures_util::StreamExt;
use serde_json::json;
use support::{Request, Response, Server, control, werk};
use werk_worker::{Message, Role, providers::*};

fn config(server: &Server, streaming: bool) -> WerkConfig {
    let mut config = WerkConfig::local("fixture-model");
    config.base_url = server.origin.clone();
    config.stream = streaming;
    config.capabilities.context_tokens = Some(4096);
    config.capabilities.streaming = Support::Supported;
    config
}

fn prompt() -> ModelRequest {
    ModelRequest {
        messages: vec![Message::text(Role::User, "transport-marker")],
        reasoning_tier: None,
        tools: vec![],
        max_output_tokens: 32,
        output_schema: None,
    }
}

fn completion(stream: bool) -> Response {
    if stream {
        Response {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_vec(),
        }
    } else {
        Response::json(
            json!({"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}),
        )
    }
}

async fn reply(provider: &WerkProvider) {
    let mut stream = provider.stream(prompt()).await.unwrap();
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let ModelDelta::Text { text: delta } = item.unwrap() {
            text.push_str(&delta);
        }
    }
    assert_eq!(text, "ok");
}

#[tokio::test]
async fn werk_chat_uses_negotiated_routes_for_json_and_sse() {
    for streaming in [false, true] {
        let server = Server::start(move |r| match r.path.as_str() {
            "/werk/v1/chat/completions" => completion(streaming),
            _ => werk(r),
        })
        .await;
        let provider = WerkProvider::connect(config(&server, streaming), Some("werk-key".into()))
            .await
            .unwrap();
        reply(&provider).await;
        let reqs = server.requests.lock().unwrap();
        assert!(reqs.iter().all(|r| {
            r.path.starts_with("/werk/v1/")
                && r.raw
                    .to_ascii_lowercase()
                    .contains("x-werk-protocol-version: 1.0")
        }));
        let chat = reqs
            .iter()
            .find(|r| r.path == "/werk/v1/chat/completions")
            .unwrap();
        assert_eq!(chat.body["messages"][0]["content"], "transport-marker");
        assert!(
            chat.raw
                .to_ascii_lowercase()
                .contains("authorization: bearer werk-key")
        );
        assert_eq!(reqs[0].path, "/werk/v1/info");
        assert_eq!(reqs[1].path, "/werk/v1/capabilities");
        assert_eq!(reqs[2].path, "/werk/v1/models");
    }
}

#[tokio::test]
async fn missing_capability_stops_before_catalog_or_prompt() {
    let server = Server::raw(|r| if r.path == "/werk/v1/capabilities" {
        Response::json(json!({"protocol":{"major":1,"minor":0},"request_id":"req_missing","data":{"capabilities":[]}}))
    } else { werk(r) }).await;
    let error = WerkProvider::connect(config(&server, false), None)
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "unsupported_capability");
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.path.clone())
            .collect::<Vec<_>>(),
        ["/werk/v1/info", "/werk/v1/capabilities"]
    );
}

#[tokio::test]
async fn raw_json_and_unversioned_sse_are_refused_on_werk() {
    for streaming in [false, true] {
        let server = Server::raw(move |r| match r.path.as_str() {
            "/werk/v1/models" => control(json!({"data":[{"id":"fixture-model"}]})),
            "/werk/v1/chat/completions" => completion(streaming),
            _ => werk(r),
        })
        .await;
        let provider = WerkProvider::connect(config(&server, streaming), None)
            .await
            .unwrap();
        let error = provider.stream(prompt()).await.err().unwrap();
        assert!(matches!(
            error.code.as_str(),
            "invalid_envelope" | "protocol_mismatch"
        ));
        assert!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| r.path.starts_with("/werk/v1/"))
        );
    }
}

fn third_party_reply(r: &Request) -> Response {
    match r.path.as_str() {
        "/v1/models" => Response::json(json!({"object":"list","data":[{"id":"fixture-model"}]})),
        "/v1/chat/completions" => completion(r.body["stream"] == true),
        _ => Response {
            status: 404,
            headers: vec![],
            body: vec![],
        },
    }
}

#[tokio::test]
async fn explicit_third_party_uses_only_its_origin_and_credential() {
    for streaming in [false, true] {
        let server = Server::raw(third_party_reply).await;
        let provider = WerkProvider::connect_openai_compatible(
            config(&server, streaming),
            Some("frontier-key".into()),
        )
        .await
        .unwrap();
        reply(&provider).await;
        assert!(provider.identity().starts_with("openai-compatible:"));
        let reqs = server.requests.lock().unwrap();
        assert_eq!(
            reqs.iter().map(|r| &*r.path).collect::<Vec<_>>(),
            ["/v1/models", "/v1/chat/completions"]
        );
        assert!(reqs.iter().all(|r| {
            let lower = r.raw.to_ascii_lowercase();
            !lower.contains("x-werk-") && lower.contains("authorization: bearer frontier-key")
        }));
        assert!(reqs[1].body.get("werk").is_none());
    }
}

#[tokio::test]
async fn cli_external_config_selects_only_the_requested_provider() {
    let server = Server::raw(third_party_reply).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("provider.json");
    std::fs::write(&path, serde_json::to_vec(&config(&server, false)).unwrap()).unwrap();
    let cli = env!("CARGO_BIN_EXE_ww");
    let output = tokio::process::Command::new(cli)
        .args(["--openai-compatible-config"])
        .arg(&path)
        .arg("diagnose")
        .current_dir(dir.path())
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let conflict = tokio::process::Command::new(cli)
        .arg("--werk-config")
        .arg(&path)
        .arg("--openai-compatible-config")
        .arg(&path)
        .arg("diagnose")
        .current_dir(dir.path())
        .output()
        .await
        .unwrap();
    assert!(!conflict.status.success());
    let reqs = server.requests.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/v1/models");
}

#[tokio::test]
async fn werk_errors_require_an_envelope_and_respect_its_retry_flag() {
    for (status, versioned, retryable, expected) in [
        (400, true, false, "werk_protocol_invalid_request"),
        (429, true, true, "werk_protocol_unavailable"),
        (503, true, false, "werk_protocol_unavailable"),
        (503, false, false, "protocol_mismatch"),
        (401, false, false, "auth_error"),
        (403, false, false, "auth_error"),
    ] {
        let server = Server::raw(move |r| match r.path.as_str() {
            "/werk/v1/models" => control(json!({"data":[{"id":"fixture-model"}]})),
            "/werk/v1/chat/completions" => {
                let error = json!({"code":if status == 400 {"invalid_request"} else {"unavailable"},
                    "message":"private-test-key was rejected","retryable":retryable});
                let mut result = Response::json(if versioned {
                    json!({"protocol":{"major":1,"minor":0},"request_id":"req_err","error":error})
                } else {
                    json!({"error":error})
                });
                result.status = status;
                result.headers.push(("retry-after".into(), "2".into()));
                result
            }
            _ => werk(r),
        })
        .await;
        let provider =
            WerkProvider::connect(config(&server, false), Some("private-test-key".into()))
                .await
                .unwrap();
        let error = provider.stream(prompt()).await.err().unwrap();
        assert_eq!(error.code, expected, "HTTP {status}, versioned={versioned}");
        assert_eq!(error.retryable, retryable);
        assert!(!error.message.contains("private-test-key"));
        if versioned {
            assert_eq!(error.retry_after_ms, Some(2000));
            assert!(error.message.contains("req_err"));
        }
        let requests = server.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.path == "/werk/v1/chat/completions")
                .count(),
            1
        );
        assert!(requests.iter().all(|r| r.path.starts_with("/werk/v1/")));
    }
}
