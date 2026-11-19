mod support;

use serde_json::{Value, json};
use std::sync::Arc;
use support::{Request, Response, Server, control};
use werk_worker::{
    policy::{HostPolicy, ToolPermission},
    profiles::AgentProfile,
    providers::{
        CapabilityStatus, RuntimeCapability, ScriptedProvider, ScriptedTurn, WerkClient, WerkConfig,
    },
    tools::{ToolRegistry, WerkNativeTool},
    *,
};

fn config(origin: &str) -> WerkConfig {
    WerkConfig {
        base_url: origin.into(),
        ..WerkConfig::local("installed-model")
    }
}

fn capabilities() -> Vec<RuntimeCapability> {
    vec![RuntimeCapability {
        id: "api.tools".into(),
        status: CapabilityStatus::Supported,
        operations: vec!["list".into(), "tasks".into(), "call".into()],
        detail: "Fixture media gateway".into(),
    }]
}

fn media(request: &Request) -> Response {
    assert!(
        request
            .raw
            .to_ascii_lowercase()
            .contains("x-werk-protocol-version: 1.0\r\n")
    );
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/werk/v1/tools") => control(json!({
            "object":"werk.tools",
            "tools":[
                {"type":"function","function":{"name":"image_generation","parameters":{"type":"object","properties":{"model":{"type":"string"}},"required":["model"]}}},
                {"type":"function","function":{"name":"get_job","parameters":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}}}
            ],
            "call_url":"/werk/v1/tools/call"
        })),
        ("GET", "/werk/v1/tasks") => control(json!({
            "object":"werk.capabilities","models":[{"id":"installed-model","available_tasks":["image_generation"]}]
        })),
        ("POST", "/werk/v1/tools/call") => {
            assert_eq!(request.body["type"], "function");
            let function = &request.body["function"];
            let args: Value =
                serde_json::from_str(function["arguments"].as_str().unwrap()).unwrap();
            let status = match function["name"].as_str().unwrap() {
                "image_generation" => {
                    assert_eq!(args, json!({"model":"installed-model"}));
                    "queued"
                }
                "get_job" => {
                    assert_eq!(args, json!({"id":"job_a"}));
                    "completed"
                }
                name => panic!("Unexpected media task {name}"),
            };
            control(json!({
                "object":"werk.tool_result","name":function["name"],"tool_call_id":request.body["id"],
                "result":{"id":"job_a","url":"/werk/v1/jobs/job_a","status":status}
            }))
        }
        other => panic!("Unexpected request {other:?}"),
    }
}

fn tool(name: &str, args: Value) -> ScriptedTurn {
    ScriptedTurn::calls(
        "",
        vec![ToolCall {
            id: id("call"),
            name: name.into(),
            arguments: args,
        }],
    )
}

#[tokio::test]
async fn media_result_allows_a_followup_only_for_the_sessions_accepted_job() {
    let server = Server::start(media).await;
    let client = WerkClient::new(&config(&server.origin), None).unwrap();
    let names = vec!["image_generation".into(), "get_job".into()];
    let tools = WerkNativeTool::discover(client, &names, &capabilities(), false)
        .await
        .unwrap();
    let mut registry = ToolRegistry::builtins().unwrap();
    for t in tools {
        registry.register(t).unwrap();
    }
    let provider = ScriptedProvider::new(vec![
        tool("werk.image_generation", json!({"model":"unavailable"})),
        tool("werk.image_generation", json!({"model":"installed-model"})),
        tool("werk.get_job", json!({"id":"foreign_job"})),
        tool("werk.get_job", json!({"id":"job_a"})),
        ScriptedTurn::text("done"),
    ]);
    let engine = WorkerEngine::builder(Arc::new(provider))
        .tools(registry)
        .harness(werk_worker::harness::HarnessConfig {
            coding_checks: false,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let enabled: Vec<String> = names.iter().map(|n| format!("werk.{n}")).collect();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "media".into(),
                prompt: "media fixture".into(),
                tools: enabled.clone(),
                skills: vec![],
            },
            dir.path(),
            HostPolicy {
                tool_permissions: enabled
                    .into_iter()
                    .map(|n| (n, ToolPermission::Allow))
                    .collect(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Generate and poll",
            "media_command",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let records = server.requests.lock().unwrap().clone();
    assert_eq!(
        records.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        [
            "/werk/v1/tools",
            "/werk/v1/tasks",
            "/werk/v1/tools/call",
            "/werk/v1/tools/call"
        ]
    );
    assert_eq!(records[2].body["function"]["name"], "image_generation");
    assert_eq!(records[3].body["function"]["name"], "get_job");
    engine.close().await.unwrap();
}

#[tokio::test]
async fn native_discovery_requires_the_advertised_executable_gateway() {
    let server =
        Server::start(|_| panic!("An unavailable capability must not make a request")).await;
    let client = WerkClient::new(&config(&server.origin), None).unwrap();
    let names = vec!["image_generation".into()];
    for (status, missing_call, experimental) in [
        (CapabilityStatus::Supported, true, false),
        (CapabilityStatus::Unsupported, false, true),
        (CapabilityStatus::Experimental, false, false),
    ] {
        let mut caps = capabilities();
        caps[0].status = status;
        if missing_call {
            caps[0].operations.pop();
        }
        let error = WerkNativeTool::discover(client.clone(), &names, &caps, experimental)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "unsupported_capability");
    }
    assert!(
        WerkNativeTool::discover(client, &[], &[], false)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(server.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn media_jobs_and_bounded_binary_outputs_use_the_versioned_contract() {
    let server = Server::start(|r| {
        let headers = r.raw.to_ascii_lowercase();
        assert!(headers.lines().any(|v| v=="x-werk-protocol-version: 1.0"));
        if r.path == "/werk/v1/jobs/job_a" {
            let state=match r.method.as_str() {"GET"=>"queued","DELETE"=>"cancelled",other=>panic!("Unexpected job method {other}")};
            return control(json!({"id":"job_a","status":state}));
        }
        let artifact = r.path.strip_prefix("/werk/v1/outputs/").unwrap();
        assert_eq!(r.method,"GET");
        assert!(headers.lines().any(|v| v.starts_with("accept:") && v.contains("application/octet-stream") && v.contains("application/json")));
        if artifact == "unknown" {
            let mut result=control(json!({}));
            result.status=404;
            result.body=serde_json::to_vec(&json!({"protocol":{"major":1,"minor":0},"request_id":"req_test","error":{"code":"not_found","message":"fixture output absent","retryable":false}})).unwrap();
            return result;
        }
        let mut response=Response{status:200, headers:vec![
            ("content-type".into(),"application/octet-stream".into()),
            ("x-werk-protocol-version".into(),"1.0".into()),
            ("x-werk-request-id".into(),"output_1".into())
        ], body:vec![0, 255, 17]};
        match artifact {
            "bytes"=>{},
            "missing_id"=>{response.headers.pop();},
            "wrong_version"=>response.headers[1].1="2.0".into(),
            "wrong_type"=>response.headers[0].1="application/json".into(),
            "large"=>response.body=vec![42;513],
            other=>panic!("Unexpected output {other}"),
        }
        response
    }).await;
    let mut cfg = config(&server.origin);
    cfg.max_body_bytes = 512;
    let client = WerkClient::new(&cfg, None).unwrap();
    assert_eq!(
        client.job("job_a", false).await.unwrap()["status"],
        "queued"
    );
    assert_eq!(
        client.job("job_a", true).await.unwrap()["status"],
        "cancelled"
    );
    assert_eq!(client.output("bytes").await.unwrap(), [0, 255, 17]);
    for (id, code) in [
        ("missing_id", "invalid_envelope"),
        ("wrong_version", "invalid_envelope"),
        ("wrong_type", "invalid_envelope"),
        ("large", "body_limit"),
        ("unknown", "werk_protocol_not_found"),
    ] {
        assert_eq!(client.output(id).await.unwrap_err().code, code, "{id}");
    }
    assert_eq!(
        client.job("bad/id", true).await.unwrap_err().code,
        "invalid_id"
    );
    assert_eq!(
        client.output("bad/id").await.unwrap_err().code,
        "invalid_id"
    );
    assert_eq!(server.requests.lock().unwrap().len(), 8);
}
