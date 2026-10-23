mod support;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::*;
use werk_worker::{policy::*, profiles::AgentProfile, providers::*, *};

fn control_request(
    request: &Request,
    context: usize,
    output: usize,
    reasoning: Option<usize>,
) -> Option<Response> {
    Some(match request.path.as_str() {
        "/werk/v1/capabilities" => control(json!({"capabilities":[
            {"id":"api.chat.limits","status":"supported","detail":"fixture","operations":["limits"]},
            {"id":"api.chat.reasoning","status":"supported","detail":"fixture","operations":["profile"]},
            {"id":"api.chat.token_count","status":"supported","detail":"fixture","operations":["count_tokens"]}]})),
        p if p.starts_with("/werk/v1/chat/limits?") => control(
            json!({"schema_version":1,"model":"fixture-model","context_tokens":context,"max_output_tokens":output,"max_concurrent_requests":1}),
        ),
        p if p.starts_with("/werk/v1/chat/reasoning?") => {
            let requested: usize = p
                .split("max_output_tokens=")
                .nth(1)
                .unwrap()
                .split('&')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            control(
                json!({"schema_version":1,"model":"fixture-model","supported":true,"default_tier":"selected","default_adjusted_to_limits":false,
                "limits":{"context_tokens":context,"output_tokens":requested.min(output),"reasoning_tokens":reasoning,"answer_reserve_tokens":512},
                "tiers":[{"id":"selected","label":"Selected","available":true,"reasoning_tokens":reasoning,"minimum_output_tokens":1},
                {"id":"off","label":"Off","available":true,"reasoning_tokens":0,"minimum_output_tokens":1}]}),
            )
        }
        "/werk/v1/chat/count_tokens" => {
            control(json!({"model":"fixture-model","input_tokens":128}))
        }
        _ => return None,
    })
}
fn tool(name: &str, arguments: Value) -> Response {
    Response::json(
        json!({"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":id("call"),"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]}),
    )
}
async fn setup(
    server: &Server,
    dir: &std::path::Path,
    names: &[&str],
    coding: bool,
) -> (WorkerEngine, SessionHandle) {
    let provider = WerkProvider::connect(
        WerkConfig {
            base_url: server.origin.clone(),
            stream: false,
            capabilities: ModelCapabilities {
                tool_calling: Support::Supported,
                ..Default::default()
            },
            ..WerkConfig::local("fixture-model")
        },
        None,
    )
    .await
    .unwrap();
    let engine = WorkerEngine::builder(Arc::new(provider))
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: if coding { "coding-basic" } else { "inspection" }.into(),
                prompt: "Complete and verify the task".into(),
                tools: names.iter().map(|s| s.to_string()).collect(),
                skills: vec![],
            },
            dir,
            HostPolicy {
                workspace_writable: coding,
                tool_permissions: names
                    .iter()
                    .map(|n| (n.to_string(), ToolPermission::Allow))
                    .collect(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    (engine, session)
}

#[tokio::test]
async fn protocol_budgets_and_hash_guarded_section_insertion() {
    for (context, output, reasoning, explicit_off) in [
        (16384, 4096, Some(2048), false),
        (8192, 1024, Some(512), false),
        (8192, 1024, None, false),
        (8192, 1024, Some(512), true),
        (8192, 512, Some(256), false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let source = "header\n".to_owned() + &"const value = 1;\n".repeat(600);
        std::fs::write(dir.path().join("game.js"), &source).unwrap();
        let section = format!("/*{}*/\n", "x".repeat(500));
        let changed = source.replacen("header\n", &format!("header\n{section}"), 1);
        let final_text = changed.replacen(&section, &format!("{section}next\n"), 1);
        let step = AtomicUsize::new(0);
        let original_hash = hash(&source);
        let changed_hash = hash(&changed);
        let server = Server::start(move |request| {
            if let Some(response)=control_request(request,context,output,reasoning){return response;}
            if request.path=="/v1/chat/completions" {
                return match step.fetch_add(1,Ordering::SeqCst) {
                    0 => tool("files.read",json!({"path":"game.js"})),
                    1 => tool("files.insert",json!({"path":"game.js","expected_hash":original_hash,"after_line":1,"content":section})),
                    2 => tool("files.insert",json!({"path":"game.js","expected_hash":original_hash,"after_line":1,"content":"must not execute"})),
                    3 => tool("files.insert",json!({"path":"game.js","expected_hash":changed_hash,"after_line":2,"content":"next\n"})),
                    _ => panic!("unexpected inference"),
                };
            }
            werk(request)
        }).await;
        let (engine, session) =
            setup(&server, dir.path(), &["files.read", "files.insert"], true).await;
        if explicit_off {
            session
                .set_reasoning_tier("off", &Limits::default())
                .await
                .unwrap();
        }
        let run = session
            .submit(
                "Add complete sections",
                "start",
                Limits {
                    max_steps: 4,
                    reserve_output_tokens: 4096,
                    ..Limits::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("game.js")).unwrap(),
            final_text
        );
        let snapshot = session.snapshot().await.unwrap();
        let calls = &snapshot.run(&run.id).unwrap().invocations;
        let read = calls[0].result.as_ref().unwrap();
        assert!(read.data["next_start_line"].as_u64().unwrap() > 1);
        assert_eq!(
            calls[1].result.as_ref().unwrap().data["next_insert_after_line"],
            2
        );
        assert_eq!(
            calls[2].result.as_ref().unwrap().error_code.as_deref(),
            Some("write_conflict")
        );
        assert_eq!(calls[3].result.as_ref().unwrap().status, Outcome::Succeeded);
        {
            let requests = server.requests.lock().unwrap();
            for request in requests.iter().filter(|r| r.path == "/v1/chat/completions") {
                assert_eq!(request.body["max_completion_tokens"], output);
                let definition = &request.body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|t| t["function"]["name"] == "files.insert")
                    .unwrap()["function"];
                let budget = &definition["parameters"]["x-werk-file-io"];
                assert_eq!(budget["context_tokens"], context);
                assert_eq!(budget["output_tokens"], output);
                assert_eq!(
                    budget["reasoning_tokens"],
                    json!(if explicit_off { Some(0) } else { reasoning })
                );
                assert_eq!(budget["target_is_estimate"], true);
                assert!(budget["target_new_characters"].as_u64().unwrap() > 0);
                assert_eq!(
                    definition["parameters"]["properties"]["content"]["maxLength"], 65536,
                    "estimated chunk target must not become another false hard limit"
                );
                assert!(
                    serde_json::to_vec(&read.data).unwrap().len()
                        <= budget["read_page_bytes"].as_u64().unwrap() as usize
                );
            }
        }
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn long_unicode_line_paginates_forward_without_losing_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let original = format!("{}\nlast line\n", "🧱".repeat(6000));
    std::fs::write(dir.path().join("large.txt"), &original).unwrap();
    let server = Server::start(|request| {
        if let Some(response) = control_request(request, 16384, 4096, Some(2048)) {
            return response;
        }
        if request.path == "/v1/chat/completions" {
            let last = request.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|m| m["role"] == "tool")
                .and_then(|m| serde_json::from_str::<Value>(m["content"].as_str()?).ok());
            if let Some(last) = last {
                if let Some(next) = last["data"]["next_start_byte"].as_u64() {
                    return tool("files.read", json!({"path":"large.txt","start_byte":next}));
                }
                return werk(request);
            }
            return tool("files.read", json!({"path":"large.txt"}));
        }
        werk(request)
    })
    .await;
    let (engine, session) = setup(&server, dir.path(), &["files.read"], false).await;
    let run = session
        .submit(
            "Read the large file",
            "start",
            Limits {
                max_steps: 30,
                reserve_output_tokens: 4096,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let data = session.snapshot().await.unwrap();
    let mut combined = String::new();
    for i in &data.run(&run.id).unwrap().invocations {
        let result = &i.result.as_ref().unwrap().data;
        assert_eq!(
            result["start_byte"].as_u64().unwrap() as usize,
            combined.len()
        );
        assert_eq!(result["sha256"], hash(&original));
        let content = result["content"].as_str().unwrap();
        assert!(!content.is_empty());
        combined.push_str(content);
        if let Some(next) = result["next_start_byte"].as_u64() {
            assert_eq!(next as usize, combined.len());
        }
    }
    assert_eq!(combined, original);
    engine.close().await.unwrap();
}

#[tokio::test]
async fn context_reduction_refreshes_reasoning_and_counts_the_dispatched_tools() {
    let server = Server::start(|request| {
        if request.path == "/werk/v1/chat/count_tokens" {
            return control(json!({"model":"fixture-model","input_tokens":12000}));
        }
        let reasoning = request
            .path
            .split("max_output_tokens=")
            .nth(1)
            .and_then(|n| n.parse::<usize>().ok())
            .map(|n| n / 2);
        if let Some(response) = control_request(request, 16384, 4096, reasoning) {
            return response;
        }
        werk(request)
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (engine, session) = setup(&server, dir.path(), &["files.read"], false).await;
    let run = session
        .submit(
            "Inspect",
            "start",
            Limits {
                reserve_output_tokens: 4096,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    {
        let requests = server.requests.lock().unwrap();
        let chat = requests
            .iter()
            .find(|r| r.path == "/v1/chat/completions")
            .unwrap();
        let output = chat.body["max_completion_tokens"].as_u64().unwrap();
        assert!(output < 4096);
        let budget = &chat.body["tools"][0]["function"]["parameters"]["x-werk-file-io"];
        assert_eq!(budget["output_tokens"], output);
        assert_eq!(budget["reasoning_tokens"], output / 2);
        assert!(
            requests
                .iter()
                .any(|r| r.path == "/werk/v1/chat/count_tokens"
                    && r.body["tools"] == chat.body["tools"]
                    && r.body["messages"] == chat.body["messages"])
        );
        let profiles: Vec<_> = requests
            .iter()
            .filter(|r| r.path.starts_with("/werk/v1/chat/reasoning?"))
            .collect();
        let distinct: std::collections::BTreeSet<_> = profiles.iter().map(|r| &r.path).collect();
        assert_eq!(
            profiles.len(),
            distinct.len(),
            "one discovery per output budget in a planning pass"
        );
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn insert_preserves_boundaries_and_rejects_invalid_positions() {
    for (original, line, section, expected, cursor) in [
        ("", 0, "first\n", Some("first\n"), Some(1)),
        ("tail\r\n", 0, "head\r\n", Some("head\r\ntail\r\n"), Some(1)),
        ("head", 1, "\ntail", Some("head\ntail"), Some(2)),
        (
            "head\ntail\n",
            1,
            "prefix",
            Some("head\nprefixtail\n"),
            None,
        ),
        ("head\n", 2, "invalid", None, None),
    ] {
        let original_hash = hash(original);
        let server = Server::start(move |request| {
            if let Some(response) = control_request(request, 16384, 4096, Some(2048)) {
                return response;
            }
            if request.path == "/v1/chat/completions" {
                return tool("files.insert", json!({"path":"file.txt","expected_hash":original_hash,"after_line":line,"content":section}));
            }
            werk(request)
        }).await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), original).unwrap();
        let (engine, session) = setup(&server, dir.path(), &["files.insert"], true).await;
        let run = session
            .submit(
                "Insert section",
                "start",
                Limits {
                    max_steps: 1,
                    ..Limits::default()
                },
                None,
            )
            .await
            .unwrap();
        run.wait().await.unwrap();
        let snapshot = session.snapshot().await.unwrap();
        let receipt = snapshot.run(&run.id).unwrap().invocations[0]
            .result
            .as_ref()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("file.txt")).unwrap(),
            expected.unwrap_or(original)
        );
        if expected.is_some() {
            assert_eq!(receipt.status, Outcome::Succeeded);
            assert_eq!(receipt.data["next_insert_after_line"], json!(cursor));
        } else {
            assert_eq!(receipt.error_code.as_deref(), Some("write_conflict"));
        }
        engine.close().await.unwrap();
    }
}
