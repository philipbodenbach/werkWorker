#![cfg(target_os = "linux")]
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use werk_worker::{policy::*, profiles::AgentProfile, providers::*, *};

const HTML: &str = r#"<!doctype html><body>
<span id="stepTag">Step 01</span><button id="nextBtn">Next</button>
<button id="tab-example" role="tab">Example</button><div id="panelExample" hidden>Example</div>
<script>
document.getElementById('nextBtn').onclick=()=>document.getElementById('stepTag').textContent='Step 02';
document.getElementById('tab-example').onclick=()=>document.getElementById('panelExample').hidden=false;
</script>"#;

fn turn(name: &str, arguments: Value) -> ScriptedTurn {
    ScriptedTurn::calls(
        "",
        vec![ToolCall {
            id: id("call"),
            name: name.into(),
            arguments,
        }],
    )
}
fn plan(status: &str) -> ScriptedTurn {
    turn(
        "task.plan",
        json!({"steps":[{"id":"interactions","description":"Verify navigation and tabs",
        "status":status,"acceptance_criteria":["Next advances the step","Example opens the panel"],
        "verification_method":"Use browser.check clicks and assertions on the rendered page"}]}),
    )
}
fn latest_tool(request: &ModelRequest) -> Value {
    serde_json::from_str(
        &request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .unwrap()
            .content,
    )
    .unwrap()
}
fn cycle(request: &ModelRequest) -> Value {
    request
        .messages
        .iter()
        .filter_map(|m| serde_json::from_str::<Value>(&m.content).ok())
        .find(|v| v["event"] == "coding_task_state")
        .unwrap()["data"]["work_cycle"]
        .clone()
}
fn check(actions: Value, replaces: Value) -> ScriptedTurn {
    let mut args = json!({"path":"index.html","screenshot":false,"actions":actions,
        "verification":{"criteria":["Navigation and tabs work"],"paths":["index.html"]}});
    if replaces.is_string() {
        args["verification"]["replaces"] = replaces;
    }
    turn("browser.check", args)
}

// This provider reacts to the real engine/tool messages. It cannot supply fake
// browser receipts or erase failures; selectors/assertions run in Chromium.
struct RecoveryProvider(AtomicUsize);
#[async_trait::async_trait]
impl ModelProvider for RecoveryProvider {
    fn identity(&self) -> String {
        "check-recovery-probe".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let step = self.0.fetch_add(1, Ordering::SeqCst);
        let response = match step {
            0 => {
                assert!(!request.tools.iter().any(|t| t.name == "files.edit"));
                turn(
                    "files.edit",
                    json!({"path":"index.html","expected_hash":hash(HTML),"old":"Step 01","new":"Wrong edit"}),
                )
            }
            1 => {
                assert_eq!(latest_tool(&request)["data"]["phase"], "plan");
                plan("in_progress")
            }
            2 => check(
                json!([{"type":"expect","expression":"document.querySelectorAll('button').length + ':5'"}]),
                Value::Null,
            ),
            3 => {
                let result = latest_tool(&request);
                assert_eq!(result["data"]["failure_origin"], "check");
                assert_eq!(result["data"]["assertions"], 0);
                assert!(result["data"]["page_errors"].is_null());
                assert!(
                    result["data"]["errors"]
                        .to_string()
                        .contains("Expected boolean")
                );
                check(
                    json!([{"type":"expect","expression":"document.querySelector('#panelStep').textContent === 'Step 01'"}]),
                    cycle(&request)["unresolved_check"]["source_invocation"].clone(),
                )
            }
            4 => {
                let result = latest_tool(&request);
                assert_eq!(result["data"]["failed_action"]["failure_origin"], "check");
                assert!(result["data"]["page_errors"].is_null());
                assert!(
                    result["data"]["element_ids"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("stepTag"))
                );
                turn(
                    "files.read",
                    json!({"path":"index.html","start_byte":0,"start_line":1}),
                )
            }
            5 => {
                assert!(
                    latest_tool(&request)["data"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("do not combine")
                );
                assert_eq!(cycle(&request)["check_repair_due"], true);
                assert!(request.tools.iter().any(|t| t.name == "browser.check"));
                assert!(!request.tools.iter().any(|t| matches!(
                    t.name.as_str(),
                    "files.edit" | "files.search" | "process.run"
                )));
                // A hallucinated edit must not escape the offered repair phase.
                turn(
                    "files.edit",
                    json!({"path":"index.html","expected_hash":hash(HTML),"old":"Step 01","new":"Wrong edit"}),
                )
            }
            6 => {
                assert_eq!(latest_tool(&request)["data"]["phase"], "check_repair");
                check(
                    json!([
                        {"type":"expect_text","selector":"#stepTag","text":"Step 01"},
                        {"type":"click","selector":"#nextBtn"},
                        {"type":"expect","expression":"document.querySelector('#stepTag').textContent === 'Step 02'"},
                        {"type":"click","selector":"#tab-example"},
                        {"type":"expect_visible","selector":"#panelExample"}
                    ]),
                    cycle(&request)["unresolved_check"]["source_invocation"].clone(),
                )
            }
            7 => {
                assert_eq!(latest_tool(&request)["data"]["passed"], true);
                assert!(cycle(&request)["unresolved_check"].is_null());
                plan("completed")
            }
            _ => ScriptedTurn::text("Verified navigation and tabs through corrected checks"),
        };
        ScriptedProvider::new(vec![response]).stream(request).await
    }
}

#[tokio::test]
async fn engine_preserves_validation_details_and_requires_explicit_plan_criteria() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), HTML).unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        turn("task.plan", json!({"steps":[]})),
        turn(
            "task.plan",
            json!({"steps":[{"id":"verify","description":"Check page","status":"completed"}]}),
        ),
        plan("completed"),
        turn(
            "files.read",
            json!({"path":"index.html","start_line":1,"start_byte":0}),
        ),
        turn("files.read", json!({"path":"index.html","start_line":0})),
        turn(
            "files.read",
            json!({"path":"index.html","start_line":1,"end_line":2}),
        ),
        ScriptedTurn::text("Inspected the fixture"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let names = ["task.plan", "files.read", "files.edit"];
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "coding-basic".into(),
                prompt: "Inspect the fixture".into(),
                tools: names.map(str::to_owned).to_vec(),
                skills: vec![],
            },
            dir.path(),
            HostPolicy {
                workspace_writable: true,
                tool_permissions: names.map(|n| (n.into(), ToolPermission::Allow)).into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("Inspect the fixture", "start", Limits::default(), None)
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    let state = run.status().await.unwrap();
    for index in [0, 1, 3, 4] {
        let error = state.invocations[index].result.as_ref().unwrap();
        assert_ne!(error.status, Outcome::Succeeded);
        assert!(
            error.data["message"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "{error:?}"
        );
    }
    assert!(
        state.invocations[3].result.as_ref().unwrap().data["message"]
            .as_str()
            .unwrap()
            .contains("do not combine")
    );
    assert_eq!(
        state.invocations[5].result.as_ref().unwrap().status,
        Outcome::Succeeded
    );
    for request in provider.requests() {
        werk_worker::context::validate_messages(&request.messages).unwrap();
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn real_browser_repairs_bad_checks_and_preserves_application_through_engine() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), HTML).unwrap();
    let engine = WorkerEngine::builder(Arc::new(RecoveryProvider(AtomicUsize::new(0))))
        .build()
        .await
        .unwrap();
    let names = [
        "files.read",
        "files.search",
        "files.edit",
        "task.plan",
        "browser.check",
        "process.run",
    ];
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "coding-basic".into(),
                prompt: "Verify the page".into(),
                tools: names.map(str::to_owned).to_vec(),
                skills: vec![],
            },
            dir.path(),
            HostPolicy {
                workspace_writable: true,
                tool_permissions: names.map(|n| (n.into(), ToolPermission::Allow)).into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Verify navigation and tabs; preserve the working page",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("index.html")).unwrap(),
        HTML
    );
    let state = run.status().await.unwrap();
    assert_eq!(
        state
            .invocations
            .iter()
            .filter(|i| i.call.name == "browser.check")
            .count(),
        3
    );
    assert!(
        state
            .invocations
            .iter()
            .filter(|i| i.call.name == "files.edit")
            .all(|i| i.result.as_ref().unwrap().status != Outcome::Succeeded)
    );
    engine.close().await.unwrap();
}
