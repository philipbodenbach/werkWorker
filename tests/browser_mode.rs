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
    captured: Mutex<Vec<ModelRequest>>,
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
        self.captured.lock().unwrap().push(request.clone());
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
async fn local_preview_reuses_browser_and_manual_navigation_respects_site_rules() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let (dir, engine, session, provider, client, panel) = setup().await;
    std::fs::write(dir.path().join("index.html"), "<body>First</body>").unwrap();
    std::fs::write(dir.path().join("next.html"), "<body>Second</body>").unwrap();
    session.set_browser_mode(true, "enable").await.unwrap();
    provider.call("browser.open", json!({"path":"index.html"}));
    let run = session
        .submit("Preview local file", "local-open", Limits::default(), None)
        .await
        .unwrap();
    let request = approval(&client, &panel).await;
    assert_eq!(
        request["description"]["arguments"]["workspace_path"],
        "index.html"
    );
    control(&client, &panel, json!({"op":"approval","run_id":run.id,"request_id":request["request_id"],"decision":"site"})).await;
    finish(&run).await;
    let browser_id = state(&client, &panel).await["browser"]["browser_id"].clone();
    assert!(browser_id.is_string());
    provider.call("browser.open", json!({"url":"file:///workspace/next.html"}));
    let repeated = session
        .submit("Preview next page", "local-next", Limits::default(), None)
        .await
        .unwrap();
    finish(&repeated).await;
    let result = repeated.status().await.unwrap().invocations[0]
        .result
        .clone()
        .unwrap();
    assert_eq!(result.status, Outcome::Succeeded, "{result:?}");
    assert_eq!(result.data["browser_id"], browser_id);
    assert_eq!(result.data["reused_session"], true);
    // Reuse does not silently grant an additional website.
    provider.call("browser.open", json!({"url":"https://unapproved.invalid"}));
    let other = session
        .submit(
            "Request another site",
            "other-site",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    let request = approval(&client, &panel).await;
    control(&client, &panel, json!({"op":"approval","run_id":other.id,"request_id":request["request_id"],"decision":"deny"})).await;
    finish(&other).await;
    assert_eq!(
        state(&client, &panel).await["browser"]["browser_id"],
        browser_id
    );
    control(&client, &panel, json!({"op":"pause"})).await;
    control(
        &client,
        &panel,
        json!({"op":"manual","arguments":{"url":"file:///workspace/index.html"}}),
    )
    .await;
    assert_eq!(
        state(&client, &panel).await["browser"]["url"],
        "http://workspace.invalid/index.html"
    );
    session
        .set_browser_rule(
            "http://workspace.invalid",
            "browsing",
            ToolPermission::Deny,
            "deny-local",
        )
        .await
        .unwrap();
    let origin = reqwest::Url::parse(&panel)
        .unwrap()
        .origin()
        .ascii_serialization();
    let denied = client
        .post(format!("{panel}control"))
        .header("origin", origin)
        .header("x-werk-browser", "1")
        .json(&json!({"op":"manual","arguments":{"url":"file:///workspace/next.html"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 409);
    assert_eq!(
        state(&client, &panel).await["browser"]["url"],
        "http://workspace.invalid/index.html"
    );
    engine.close().await.unwrap();
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
    // Continue now requires a fresh observation before the next action.
    provider.call("browser.observe", json!({"browser_id":browser_id}));
    let refreshed = session
        .submit(
            "Inspect manual changes",
            "refresh-after-manual",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    finish(&refreshed).await;
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

#[tokio::test]
async fn main_agent_changed_pages_allow_repeated_actions_and_observations_with_vision() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let (dir, engine, session, provider, client, panel) = setup().await;
    // This fixture exposes the full host toolset and uses conservative token
    // estimation. Native backend counting is tested separately.
    let browser_limits = Limits {
        context_tokens: 65536,
        ..Limits::default()
    };
    std::fs::write(dir.path().join("counter.html"), r#"<!doctype html><title>Counter</title>
        <button id="next" onclick="setTimeout(()=>document.querySelector('output').textContent=++window.n,250)">Next</button>
        <output>0</output><script>window.n=0; document.onkeydown=()=>document.querySelector('output').textContent=++window.n;</script>"#).unwrap();
    session.set_browser_mode(true, "mode").await.unwrap();
    for category in ["browsing", "actions"] {
        session
            .set_browser_rule(
                "http://workspace.invalid",
                category,
                ToolPermission::Allow,
                &id("rule"),
            )
            .await
            .unwrap();
    }
    session
        .set_browser_settings(
            browser_policy::BrowserSettings {
                perception: browser_policy::Perception::Vision,
                ..Default::default()
            },
            "vision",
        )
        .await
        .unwrap();
    provider.call("browser.open", json!({"path":"counter.html"}));
    let opened = session
        .submit("Open counter", "open", browser_limits.clone(), None)
        .await
        .unwrap();
    finish(&opened).await;
    let opening_state = opened.status().await.unwrap();
    assert_eq!(
        opening_state.status,
        RunStatus::Completed,
        "{:?}",
        opening_state.error
    );
    let first = opened.status().await.unwrap().invocations[0]
        .result
        .clone()
        .unwrap();
    assert_eq!(first.images.len(), 1, "{first:?}");
    let browser_id = first.data["browser_id"].clone();
    for _ in 0..6 {
        provider.call(
            "browser.action",
            json!({"browser_id":browser_id,"action":"click","target":{"selector":"#next"}}),
        );
        provider.call("browser.observe", json!({"browser_id":browser_id}));
        provider.call(
            "browser.action",
            json!({"browser_id":browser_id,"action":"press","key":"ArrowRight"}),
        );
    }
    let run = session
        .submit(
            "Advance twelve times",
            "advance",
            Limits {
                max_steps: 32,
                max_tool_invocations: 40,
                ..browser_limits.clone()
            },
            None,
        )
        .await
        .unwrap();
    finish(&run).await;
    let status = run.status().await.unwrap();
    assert_eq!(status.invocations.len(), 18, "{:?} receipts={:?}", status.error,status.invocations.iter().map(|i|json!({"action":i.call.arguments["action"],"data":i.result.as_ref().map(|r| &r.data),"error":i.result.as_ref().and_then(|r|r.error_code.as_ref())})).collect::<Vec<_>>());
    for i in &status.invocations {
        let r = i.result.as_ref().unwrap();
        assert_eq!(r.status, Outcome::Succeeded, "{r:?}");
        assert_eq!(r.images.len(), 1);
        assert!(r.data["observation_id"].is_string());
        assert_eq!(r.data["tab_id"], 1);
    }
    assert!(
        status
            .invocations
            .last()
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .data["text"]
            .as_str()
            .unwrap()
            .contains("12")
    );
    {
        let requests = provider.captured.lock().unwrap();
        assert!(
            requests
                .iter()
                .any(|r| r.messages.iter().any(|m| !m.images.is_empty()))
        );
        for request in requests.iter() {
            assert!(
                request
                    .messages
                    .iter()
                    .map(|m| m.images.len())
                    .sum::<usize>()
                    <= 2
            );
        }
    }
    // Continue invalidates the old action even if no manual navigation occurred.
    control(&client, &panel, json!({"op":"pause"})).await;
    control(
        &client,
        &panel,
        json!({"op":"manual","arguments":{"action":"press","key":"ArrowRight"}}),
    )
    .await;
    control(&client, &panel, json!({"op":"resume"})).await;
    provider.call(
        "browser.action",
        json!({"browser_id":browser_id,"action":"press","key":"ArrowRight"}),
    );
    provider.call("browser.observe", json!({"browser_id":browser_id}));
    let resumed = session
        .submit(
            "Inspect after manual edit",
            "resume",
            browser_limits.clone(),
            None,
        )
        .await
        .unwrap();
    finish(&resumed).await;
    let state = resumed.status().await.unwrap();
    assert_eq!(
        state.invocations[0]
            .result
            .as_ref()
            .unwrap()
            .error_code
            .as_deref(),
        Some("browser_changed")
    );
    assert!(
        state.invocations[1].result.as_ref().unwrap().data["text"]
            .as_str()
            .unwrap()
            .contains("13")
    );
    session
        .set_browser_settings(
            browser_policy::BrowserSettings {
                perception: browser_policy::Perception::Dom,
                ..Default::default()
            },
            "dom",
        )
        .await
        .unwrap();
    provider.call(
        "browser.navigate",
        json!({"browser_id":browser_id,"url":"http://workspace.invalid/counter.html"}),
    );
    let dom = session
        .submit(
            "Navigate in DOM mode",
            "dom-run",
            browser_limits.clone(),
            None,
        )
        .await
        .unwrap();
    finish(&dom).await;
    assert!(
        dom.status().await.unwrap().invocations[0]
            .result
            .as_ref()
            .unwrap()
            .images
            .is_empty()
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn unchanged_main_agent_observations_are_still_stopped() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let (dir, engine, session, provider, _, _) = setup().await;
    std::fs::write(dir.path().join("idle.html"),"<button id='noop'>No effect</button><span aria-live='polite' id='blink'>blink</span><script>setInterval(()=>document.querySelector('#blink').textContent=String(Date.now()),80)</script>").unwrap();
    session.set_browser_mode(true, "mode").await.unwrap();
    for category in ["browsing", "actions"] {
        session
            .set_browser_rule(
                "http://workspace.invalid",
                category,
                ToolPermission::Allow,
                &id("rule"),
            )
            .await
            .unwrap();
    }
    provider.call("browser.open", json!({"path":"idle.html"}));
    let run = session
        .submit("Open", "open", Limits::default(), None)
        .await
        .unwrap();
    finish(&run).await;
    let browser_id = run.status().await.unwrap().invocations[0]
        .result
        .as_ref()
        .unwrap()
        .data["browser_id"]
        .clone();
    for _ in 0..6 {
        provider.call(
            "browser.action",
            json!({"browser_id":browser_id,"action":"click","target":{"selector":"#noop"}}),
        );
        provider.call("browser.observe", json!({"browser_id":browser_id}));
    }
    let run = session
        .submit("Check stalled page", "stalled", Limits::default(), None)
        .await
        .unwrap();
    finish(&run).await;
    let status = run.status().await.unwrap();
    assert!(
        status
            .invocations
            .iter()
            .any(|i| i.result.as_ref().is_some_and(|r| r
                .error_code
                .as_deref()
                .is_some_and(|e| e.contains("repeat"))))
            || status.status == RunStatus::Failed,
        "{status:?}"
    );
    assert!(
        status
            .invocations
            .iter()
            .filter(|i| i
                .result
                .as_ref()
                .is_some_and(|r| r.status == Outcome::Succeeded))
            .count()
            < 12
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn changing_host_perception_invalidates_an_outstanding_approval() {
    let (dir, engine, session, provider, client, panel) = setup().await;
    std::fs::write(dir.path().join("page.html"), "<body>Not opened</body>").unwrap();
    session.set_browser_mode(true, "mode").await.unwrap();
    provider.call("browser.open", json!({"path":"page.html"}));
    let run = session
        .submit("Open page", "request", Limits::default(), None)
        .await
        .unwrap();
    let request = approval(&client, &panel).await;
    assert_eq!(
        request["description"]["arguments"]["_browser_settings"]["perception"],
        "auto"
    );
    session
        .set_browser_settings(
            browser_policy::BrowserSettings {
                perception: browser_policy::Perception::Vision,
                ..Default::default()
            },
            "change",
        )
        .await
        .unwrap();
    control(&client,&panel,json!({"op":"approval","run_id":run.id,"request_id":request["request_id"],"decision":"allow"})).await;
    finish(&run).await;
    assert_eq!(
        run.status()
            .await
            .unwrap()
            .error
            .as_ref()
            .map(|e| e.code.as_str()),
        Some("browser_settings_changed")
    );
    // A failed run also closes its preview endpoint during cleanup.
    assert!(client.get(format!("{panel}state")).send().await.is_err());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn perception_settings_do_not_enable_browser_mode_or_add_permissions() {
    let (_dir, engine, session, _, client, panel) = setup().await;
    session
        .set_browser_settings(
            browser_policy::BrowserSettings {
                perception: browser_policy::Perception::Vision,
                ..Default::default()
            },
            "vision-only",
        )
        .await
        .unwrap();
    let saved = session.snapshot().await.unwrap();
    assert!(!saved.policy.browser.unwrap().enabled);
    assert!(!saved.profile.tools.contains(&"browser.open".into()));
    assert_eq!(state(&client, &panel).await["enabled"], false);
    engine.close().await.unwrap();
}
