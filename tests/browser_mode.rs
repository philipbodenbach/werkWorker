#![cfg(target_os = "linux")]
mod support;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use werk_worker::{
    browser_policy::BrowserPolicy,
    policy::{HostPolicy, PolicyDecision, ToolPermission},
    providers::*,
    *,
};

#[test]
fn site_permissions_never_promote_scripts_or_protected_fields() {
    let registry = tools::ToolRegistry::builtins().unwrap();
    let mut browser = BrowserPolicy::default();
    browser
        .set("https://example.com", "browsing", ToolPermission::Allow)
        .unwrap();
    browser
        .set("https://example.com", "actions", ToolPermission::Allow)
        .unwrap();
    assert!(browser.set("*", "scripts", ToolPermission::Allow).is_err());
    assert!(
        browser
            .set("https://example.com/path", "actions", ToolPermission::Allow)
            .is_err()
    );
    let mut args = json!({"origin":"https://example.com","allowed_origins":["https://example.com"],"new_origins":[],"browser_category":"actions"});
    assert_eq!(
        browser.evaluate("browser.action", &args),
        PolicyDecision::Allow
    );
    args["protected_field"] = json!(true);
    assert_eq!(
        browser.evaluate("browser.action", &args),
        PolicyDecision::Deny
    );
    args["protected_field"] = json!(false);
    args["browser_category"] = json!("scripts");
    // Even a hand-edited serialized Allow rule cannot preapprove scripts.
    browser
        .defaults
        .insert("scripts".into(), ToolPermission::Allow);
    assert_eq!(
        browser.evaluate("browser.evaluate", &args),
        PolicyDecision::Ask
    );
    args["browser_category"] = json!("browsing");
    args["within_session"] = json!(true);
    args["new_origins"] = json!(["https://other.example"]);
    assert_eq!(
        browser.evaluate("browser.navigate", &args),
        PolicyDecision::Ask
    );
    browser
        .set("https://other.example", "browsing", ToolPermission::Deny)
        .unwrap();
    args["allowed_origins"] = json!(["https://example.com", "https://other.example"]);
    assert_eq!(
        browser.evaluate("browser.navigate", &args),
        PolicyDecision::Deny
    );
    let mut policy = HostPolicy {
        browser: Some(browser),
        ..Default::default()
    };
    policy.hard_denied_tools.push("browser.action".into());
    assert!(!policy.can_request(&registry.get("browser.action").unwrap().definition()));
    let script = registry.get("browser.evaluate").unwrap().definition();
    let mut full = HostPolicy {
        request_approvals: true,
        ..Default::default()
    };
    full.tool_permissions
        .insert(script.name.clone(), ToolPermission::Allow);
    assert_eq!(
        full.evaluate(&script, &json!({"origin":"https://example.com"}), &[]),
        PolicyDecision::Ask
    );
}

#[derive(Default)]
struct QueueProvider {
    turns: Mutex<VecDeque<ScriptedTurn>>,
    requests: AtomicUsize,
}
impl QueueProvider {
    fn call(&self, name: &str, args: Value) {
        self.turns.lock().unwrap().push_back(ScriptedTurn::calls(
            "",
            vec![ToolCall {
                id: id("call"),
                name: name.into(),
                arguments: args,
            }],
        ));
    }
}
#[async_trait]
impl ModelProvider for QueueProvider {
    fn capabilities(&self) -> ModelCapabilities {
        ScriptedProvider::default().capabilities()
    }
    fn identity(&self) -> String {
        "browser-test".into()
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let turn = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ScriptedTurn::text("Ready."));
        ScriptedProvider::new(vec![turn]).stream(request).await
    }
}
async fn state(client: &reqwest::Client, panel: &str) -> Value {
    client
        .get(format!("{panel}state"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["host"]
        .clone()
}
async fn control(client: &reqwest::Client, panel: &str, args: Value) -> Value {
    let origin = reqwest::Url::parse(panel)
        .unwrap()
        .origin()
        .ascii_serialization();
    let response = client
        .post(format!("{panel}control"))
        .header("Origin", origin)
        .header("X-Werk-Browser", "1")
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{body}");
    assert!(body["error"].is_null(), "{body}");
    body
}
async fn approval(client: &reqwest::Client, panel: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let data = state(client, panel).await;
            if let Some(a) = data["approvals"].as_array().and_then(|a| a.first()) {
                break a.clone();
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap()
}
async fn finish(run: &RunHandle) {
    tokio::time::timeout(Duration::from_secs(60), run.wait())
        .await
        .unwrap()
        .unwrap();
}
async fn setup() -> (
    tempfile::TempDir,
    WorkerEngine,
    SessionHandle,
    Arc<QueueProvider>,
    reqwest::Client,
    String,
) {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(QueueProvider::default());
    let engine = WorkerEngine::builder(provider.clone())
        .harness(harness::HarnessConfig {
            coding_checks: false,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profiles::AgentProfile::bundled("general-readonly").unwrap(),
            dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let panel = session.browser_panel().await.unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    (dir, engine, session, provider, client, panel)
}
#[tokio::test]
async fn host_panel_rejects_cross_origin_control_and_pauses_before_inference() {
    let (_dir, engine, session, provider, client, panel) = setup().await;
    for origin in ["https://attacker.example", "null", ""] {
        let r = client
            .post(format!("{panel}control"))
            .header("Origin", origin)
            .header("X-Werk-Browser", "1")
            .json(&json!({"op":"mode","enabled":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
    }
    let origin = reqwest::Url::parse(&panel)
        .unwrap()
        .origin()
        .ascii_serialization();
    let r = client
        .post(format!("{panel}control"))
        .header("Origin", origin)
        .json(&json!({"op":"mode","enabled":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(state(&client, &panel).await["enabled"], false);
    control(&client, &panel, json!({"op":"mode","enabled":true})).await;
    let configured = session.snapshot().await.unwrap();
    assert!(configured.profile.tools.contains(&"browser.check".into()));
    assert_eq!(
        configured.policy.tool_permissions.get("browser.check"),
        Some(&ToolPermission::Ask)
    );
    assert!(
        session
            .snapshot()
            .await
            .unwrap()
            .profile
            .tools
            .contains(&"browser.evaluate".into())
    );
    control(&client, &panel, json!({"op":"pause"})).await;
    let run = session
        .submit("Wait for me", "pause-test", Limits::default(), None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while run.status().await.unwrap().status != RunStatus::WaitingInput {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(provider.requests.load(Ordering::SeqCst), 0);
    control(&client, &panel, json!({"op":"resume"})).await;
    finish(&run).await;
    assert_eq!(provider.requests.load(Ordering::SeqCst), 1);
    engine.close().await.unwrap();
    assert!(client.get(panel).send().await.is_err());
}
#[tokio::test]
async fn browser_panel_approvals_manual_password_and_scripts_work_end_to_end() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let server=support::Server::start(|_|support::Response{status:200,headers:vec![("content-type".into(),"text/html".into())],body:br#"<!doctype html><title>Login fixture</title><label>Password<input id="password" type="password"></label><label>Name<input id="name"></label><button id="save" onclick="document.querySelector('#result').textContent='Saved'">Save</button><div id="result">Ready</div>"#.to_vec()}).await;
    let (_dir, engine, session, provider, client, panel) = setup().await;
    session.set_browser_mode(true, "enable").await.unwrap();
    provider.call(
        "browser.open",
        json!({"url":server.origin,"private_origins":[server.origin]}),
    );
    let run = session
        .submit("Open the fixture", "open", Limits::default(), None)
        .await
        .unwrap();
    let request = approval(&client, &panel).await;
    assert!(server.requests.lock().unwrap().is_empty());
    control(&client,&panel,json!({"op":"approval","run_id":run.id,"request_id":request["request_id"],"decision":"site"})).await;
    finish(&run).await;
    let browser_id = state(&client, &panel).await["browser"]["browser_id"].clone();
    assert!(browser_id.is_string());
    provider.call("browser.action",json!({"browser_id":browser_id,"action":"fill","target":{"selector":"#password"},"text":"model-must-not-type"}));
    let denied = session
        .submit("Test protected field", "protected", Limits::default(), None)
        .await
        .unwrap();
    finish(&denied).await;
    assert_eq!(
        denied.status().await.unwrap().invocations[0]
            .result
            .as_ref()
            .unwrap()
            .status,
        Outcome::Denied
    );
    control(&client, &panel, json!({"op":"pause"})).await;
    control(
        &client,
        &panel,
        json!({"op":"manual","arguments":{"action":"click","target":{"selector":"#password"}}}),
    )
    .await;
    control(
        &client,
        &panel,
        json!({"op":"manual","arguments":{"action":"type","text":"HumanSecret123"}}),
    )
    .await;
    let snapshot = client
        .get(format!("{panel}state"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!snapshot.contains("HumanSecret123"));
    control(&client, &panel, json!({"op":"resume"})).await;
    provider.call("browser.observe", json!({"browser_id":browser_id}));
    let observe = session
        .submit("Read the page", "observe", Limits::default(), None)
        .await
        .unwrap();
    finish(&observe).await;
    assert!(
        !serde_json::to_string(&session.snapshot().await.unwrap())
            .unwrap()
            .contains("HumanSecret123")
    );
    // Human interaction invalidates the exact prepared revision before dispatch.
    provider.call(
        "browser.action",
        json!({"browser_id":browser_id,"action":"click","target":{"selector":"#save"}}),
    );
    let stale = session
        .submit("Review save", "stale", Limits::default(), None)
        .await
        .unwrap();
    let request = approval(&client, &panel).await;
    control(&client, &panel, json!({"op":"pause"})).await;
    control(
        &client,
        &panel,
        json!({"op":"manual","arguments":{"action":"scroll","x":0,"y":20}}),
    )
    .await;
    control(&client,&panel,json!({"op":"approval","run_id":stale.id,"request_id":request["request_id"],"decision":"allow"})).await;
    control(&client, &panel, json!({"op":"resume"})).await;
    finish(&stale).await;
    let result = stale.status().await.unwrap().invocations[0]
        .result
        .clone()
        .unwrap();
    assert_eq!(result.status, Outcome::Failed);
    assert!(
        serde_json::to_string(&result)
            .unwrap()
            .contains("browser_changed")
    );
    // Remembering this site's action category applies to a fresh invocation.
    for attempt in 0..2 {
        provider.call(
            "browser.action",
            json!({"browser_id":browser_id,"action":"click","target":{"selector":"#save"}}),
        );
        let run = session
            .submit(
                "Save fixture",
                &format!("save-{attempt}"),
                Limits::default(),
                None,
            )
            .await
            .unwrap();
        if attempt == 0 {
            let request = approval(&client, &panel).await;
            control(&client,&panel,json!({"op":"approval","run_id":run.id,"request_id":request["request_id"],"decision":"site"})).await;
        }
        finish(&run).await;
        assert_eq!(
            run.status().await.unwrap().invocations[0]
                .result
                .as_ref()
                .unwrap()
                .status,
            Outcome::Succeeded
        );
    }
    // Reusable script approval is rejected; both repeated executions ask again.
    for attempt in 0..2 {
        provider.call("browser.evaluate",json!({"browser_id":browser_id,"script":"() => { document.querySelector('#result').textContent='Script ran'; return {answer:42,secretLength:document.querySelector('#password').value.length}; }"}));
        let run = session
            .submit(
                "Run reviewed script",
                &format!("script-{attempt}"),
                Limits::default(),
                None,
            )
            .await
            .unwrap();
        let request = approval(&client, &panel).await;
        let err = run
            .respond_to_approval(
                request["request_id"].as_str().unwrap(),
                ApprovalDecision::AllowForSession,
                &format!("invalid-{attempt}"),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, "browser_script_approval");
        control(&client,&panel,json!({"op":"approval","run_id":run.id,"request_id":request["request_id"],"decision":"allow"})).await;
        finish(&run).await;
        let result = run.status().await.unwrap().invocations[0]
            .result
            .clone()
            .unwrap();
        assert_eq!(result.status, Outcome::Succeeded, "{result:?}");
        assert_eq!(result.data["script_result"]["answer"], 42);
        assert_eq!(result.data["script_result"]["secretLength"], 14);
    }
    // A new browser cannot reach the host permission panel, even with private access.
    provider.call("browser.close", json!({"browser_id":browser_id}));
    let close = session
        .submit("Close", "close", Limits::default(), None)
        .await
        .unwrap();
    finish(&close).await;
    let origin = reqwest::Url::parse(&panel)
        .unwrap()
        .origin()
        .ascii_serialization();
    provider.call(
        "browser.open",
        json!({"url":panel,"private_origins":[origin]}),
    );
    let attack = session
        .submit(
            "Test host isolation",
            "host-isolation",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    let request = approval(&client, &panel).await;
    control(&client,&panel,json!({"op":"approval","run_id":attack.id,"request_id":request["request_id"],"decision":"allow"})).await;
    finish(&attack).await;
    let result = attack.status().await.unwrap().invocations[0]
        .result
        .clone()
        .unwrap();
    assert!(!result.data.to_string().contains("Approve browser action"));
    assert!(
        result.data["blocked_requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["reason"] == "browser_control_denied"),
        "{result:?}"
    );
    engine.close().await.unwrap();
}
