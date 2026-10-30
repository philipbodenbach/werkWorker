#![cfg(target_os = "linux")]
mod support;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use werk_worker::{
    policy::{HostPolicy, PolicyDecision, ToolPermission},
    providers::{ScriptedProvider, ScriptedTurn},
    runtime::*,
    tools::*,
    *,
};

async fn context(path: &std::path::Path) -> ToolContext {
    ToolContext {
        workspace: Workspace::open(path).await.unwrap(),
        workspace_lease: None,
        session_id: id("session"),
        run_id: id("run"),
        invocation_id: id("invocation"),
        writable: false,
        cancel: CancellationToken::new(),
        sandbox: Sandbox::default(),
        jobs: Arc::default(),
        artifacts: None,
        artifact_refs: vec![],
        skills: Default::default(),
        native_jobs: vec![],
    }
}
async fn call(
    registry: &ToolRegistry,
    ctx: &ToolContext,
    name: &str,
    args: Value,
) -> Result<ToolResult> {
    let tool = registry.get(name)?;
    validate_arguments(&tool.definition().input_schema, &args)?;
    let prepared = tool.prepare(args, ctx).await?;
    tool.execute(prepared, ctx).await
}
fn assert_ok(result: &ToolResult) {
    assert_eq!(result.status, Outcome::Succeeded, "{:?}", result);
    assert!(result.data.get("error").is_none(), "{}", result.data);
}

#[test]
fn browser_tools_need_separate_external_permissions() {
    let registry = ToolRegistry::builtins().unwrap();
    let policy = werk_worker::project::Access::Process.policy();
    for name in [
        "browser.open",
        "browser.navigate",
        "browser.action",
        "browser.observe",
        "browser.close",
    ] {
        let def = registry.get(name).unwrap().definition();
        assert_eq!(def.effect, Effect::ExternalWrite);
        assert!(!policy.can_request(&def));
        let args = json!({"origin":"https://example.com","browser":"firefox"});
        assert_eq!(policy.evaluate(&def, &args, &[]), PolicyDecision::Deny);
        let mut ask = HostPolicy {
            request_approvals: true,
            ..Default::default()
        };
        assert_eq!(ask.evaluate(&def, &args, &[]), PolicyDecision::Ask);
        ask.tool_permissions
            .insert(name.into(), ToolPermission::Deny);
        assert_eq!(ask.evaluate(&def, &args, &[]), PolicyDecision::Deny);
    }
}

#[tokio::test]
async fn invalid_scope_and_action_never_launch_a_browser() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path()).await;
    let registry = ToolRegistry::builtins().unwrap();
    let open = registry.get("browser.open").unwrap();
    for url in [
        "file:///etc/passwd",
        "data:text/html,secret",
        "javascript:alert(1)",
        "https://name:password@example.com",
    ] {
        assert!(open.prepare(json!({"url":url}), &ctx).await.is_err());
    }
    assert!(
        open.prepare(
            json!({"url":"https://example.com","allowed_origins":["https://example.com/path"]}),
            &ctx
        )
        .await
        .is_err()
    );
    assert!(
        open.prepare(
            json!({"url":"https://example.com","private_origins":["http://127.0.0.1"]}),
            &ctx
        )
        .await
        .is_err()
    );
    let action = registry.get("browser.action").unwrap();
    let err = action
        .prepare(
            json!({"browser_id":"x","action":"fill","text":"secret"}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "invalid_browser_action");
    let err=action.prepare(json!({"browser_id":"x","action":"click","target":{"role":"button","name":"Save","selector":"#other"}}),&ctx).await.unwrap_err();
    assert_eq!(err.code, "invalid_browser_target");
    let prepared = open
        .prepare(
            json!({"url":"HTTPS://Example.COM/a#b","browser":"firefox"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(prepared["allowed_origins"], json!(["https://example.com"]));
    assert_eq!(prepared["private_origins"], json!([]));
}

#[tokio::test]
async fn denied_browser_approval_makes_no_network_request() {
    let server = support::Server::start(|_| support::Response::json(json!({}))).await;
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::calls(
            "",
            vec![ToolCall {
                id: id("call"),
                name: "browser.open".into(),
                arguments: json!({"url":server.origin,"private_origins":[server.origin],"browser":"firefox"}),
            }],
        ),
        ScriptedTurn::text("Denied; no browser opened."),
    ]));
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
            werk_worker::profiles::AgentProfile::bundled("automation-basic").unwrap(),
            dir.path(),
            HostPolicy {
                request_approvals: true,
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("Open browser", "start", Limits::default(), None)
        .await
        .unwrap();
    let mut events = run.events(0);
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let EventKind::ApprovalRequested { request } =
                events.next().await.unwrap().unwrap().event
            {
                break request;
            }
        }
    })
    .await
    .unwrap();
    let state = run.status().await.unwrap();
    let prepared = &state.invocations[0].prepared;
    assert_eq!(prepared["browser"], "firefox");
    assert_eq!(prepared["private_origins"], json!([server.origin]));
    assert!(server.requests.lock().unwrap().is_empty());
    run.respond_to_approval(&request.request_id, ApprovalDecision::Deny, "deny")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), run.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(server.requests.lock().unwrap().is_empty());
    assert_eq!(
        run.status().await.unwrap().invocations[0]
            .result
            .as_ref()
            .unwrap()
            .status,
        Outcome::Denied
    );
    engine.close().await.unwrap();
}

const HTML: &str = r#"<!doctype html><title>Browser fixture</title>
<label>Name <input id="name"></label><button id="save" onclick="fetch('/save',{method:'POST',body:JSON.stringify({name:document.querySelector('#name').value})}).then(r=>r.text()).then(t=>document.querySelector('#result').textContent=t)">Save</button>
<div id="result">Ready</div><a href="/next">Next</a>
<output id="painted"></output>
<button style="position:absolute;left:400px;top:500px;width:100px;height:40px" onclick="document.querySelector('#painted').textContent='Pixel clicked'">Pixel</button>
<canvas id="paint" width="220" height="120" style="position:absolute;left:20px;top:300px"></canvas>
<script>
const canvas=document.querySelector('#paint'), ctx=canvas.getContext('2d');let drawing=false;
canvas.onmousedown=e=>{drawing=true;ctx.beginPath();ctx.moveTo(e.offsetX,e.offsetY)};
canvas.onmousemove=e=>{if(drawing){ctx.strokeStyle='#e8ac10';ctx.lineWidth=8;ctx.lineTo(e.offsetX,e.offsetY);ctx.stroke()}};
canvas.onmouseup=()=>{drawing=false;document.querySelector('#painted').textContent='Canvas drawn'};
</script>"#;

#[tokio::test]
async fn real_browser_preserves_state_controls_tabs_and_enforces_scope() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        eprintln!("SKIP real browsers: set REQUIRE_BROWSER=1");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path()).await;
    if let Err(e) = ctx.sandbox.doctor().await {
        assert_ne!(
            std::env::var("REQUIRE_BROWSER").as_deref(),
            Ok("1"),
            "{}",
            e.message
        );
        eprintln!("SKIP real browser: {}", e.message);
        return;
    }
    let other =
        support::Server::start(|_| support::Response::json(json!({"unexpected":true}))).await;
    let other_origin = other.origin.clone();
    let server = support::Server::start(move |r| {
        let (status, body, headers) = match r.path.as_str() {
            "/save" => {
                assert_eq!(r.method, "POST");
                assert_eq!(r.body["name"], "Alice");
                assert!(r.raw.contains("fixture=session"));
                (200, "Saved Alice".to_string(), vec![])
            }
            "/redirect" => (
                302,
                String::new(),
                vec![("location".into(), format!("{other_origin}/secret"))],
            ),
            "/next" => (
                200,
                "<title>Next</title><body>Second page</body>".into(),
                vec![],
            ),
            _ => (
                200,
                HTML.into(),
                vec![(
                    "set-cookie".into(),
                    "fixture=session; Path=/; HttpOnly".into(),
                )],
            ),
        };
        let mut headers = headers;
        headers.push(("content-type".into(), "text/html".into()));
        support::Response {
            status,
            body: body.into_bytes(),
            headers,
        }
    })
    .await;
    let engines = std::env::var("WW_BROWSER_ENGINES").unwrap_or("chromium".into());
    for browser in engines.split(',') {
        let registry = ToolRegistry::builtins().unwrap();
        let opened = call(
            &registry,
            &ctx,
            "browser.open",
            json!({"url":server.origin,"browser":browser,"private_origins":[server.origin]}),
        )
        .await
        .unwrap();
        assert_ok(&opened);
        assert!(
            opened.data["accessibility"]
                .as_str()
                .unwrap()
                .contains("Save")
        );
        let browser_id = opened.data["browser_id"].clone();
        let fill=call(&registry,&ctx,"browser.action",json!({"browser_id":browser_id,"action":"fill","target":{"label":"Name"},"text":"Alice"})).await.unwrap();
        assert_ok(&fill);
        let save=call(&registry,&ctx,"browser.action",json!({"browser_id":browser_id,"action":"click","target":{"role":"button","name":"Save"}})).await.unwrap();
        assert_ok(&save);
        assert!(
            save.data["text"].as_str().unwrap().contains("Saved Alice"),
            "{}",
            save.data
        );
        let pixel = call(
            &registry,
            &ctx,
            "browser.action",
            json!({"browser_id":browser_id,"action":"click_at","x":450,"y":520}),
        )
        .await
        .unwrap();
        assert_ok(&pixel);
        assert!(
            pixel.data["text"]
                .as_str()
                .unwrap()
                .contains("Pixel clicked")
        );
        let painted=call(&registry,&ctx,"browser.action",json!({"browser_id":browser_id,"action":"drag","path":[{"x":40,"y":320},{"x":80,"y":360},{"x":120,"y":320}],"screenshot":true})).await.unwrap();
        assert_ok(&painted);
        assert!(
            painted.data["text"]
                .as_str()
                .unwrap()
                .contains("Canvas drawn")
        );
        assert_eq!(painted.images.len(), 1);
        assert_eq!(painted.images[0].width, 1024);
        assert_eq!(painted.images[0].height, 768);
        assert_eq!(painted.data["pointer"], json!({"x":120,"y":320}));
        let viewer = painted.data["viewer_url"].as_str().unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert!(
            client
                .get(viewer)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
                .contains("read-only preview")
        );
        let state: Value = client
            .get(format!("{viewer}state"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(state["action"], "drag");
        let frame = client
            .get(format!("{viewer}frame"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let image = image::load_from_memory(&frame).unwrap().to_rgb8();
        let p = image.get_pixel(60, 340);
        assert!(
            p[0] > 150 && p[1] > 100 && p[2] < 100,
            "Expected actual painted pixels, got {p:?}"
        );
        assert_eq!(client.post(viewer).send().await.unwrap().status(), 404);
        let viewer_origin = reqwest::Url::parse(viewer)
            .unwrap()
            .origin()
            .ascii_serialization();
        assert_eq!(
            client
                .get(format!("{viewer_origin}/wrong-token/frame"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        let observe = registry.get("browser.observe").unwrap();
        let stale = observe
            .prepare(json!({"browser_id":browser_id}), &ctx)
            .await
            .unwrap();
        let screenshot = call(
            &registry,
            &ctx,
            "browser.observe",
            json!({"browser_id":browser_id,"screenshot":true}),
        )
        .await
        .unwrap();
        assert_ok(&screenshot);
        assert_eq!(screenshot.images.len(), 1);
        assert_eq!(
            observe.execute(stale, &ctx).await.unwrap_err().code,
            "browser_changed"
        );
        let mut foreign = ctx.clone();
        foreign.session_id = id("foreign");
        assert_eq!(
            observe
                .prepare(json!({"browser_id":browser_id}), &foreign)
                .await
                .unwrap_err()
                .code,
            "browser_session_missing"
        );
        let nav = registry.get("browser.navigate").unwrap();
        assert_eq!(
            nav.prepare(json!({"browser_id":browser_id,"url":other.origin}), &ctx)
                .await
                .unwrap_err()
                .code,
            "browser_scope"
        );
        let newtab = call(
            &registry,
            &ctx,
            "browser.action",
            json!({"browser_id":browser_id,"action":"tab_new"}),
        )
        .await
        .unwrap();
        assert_ok(&newtab);
        assert_eq!(newtab.data["tabs"].as_array().unwrap().len(), 2);
        let next = call(
            &registry,
            &ctx,
            "browser.navigate",
            json!({"browser_id":browser_id,"url":format!("{}/next",server.origin)}),
        )
        .await
        .unwrap();
        assert_ok(&next);
        assert_eq!(next.data["title"], "Next");
        let back = call(
            &registry,
            &ctx,
            "browser.action",
            json!({"browser_id":browser_id,"action":"tab_select","tab_id":1}),
        )
        .await
        .unwrap();
        assert_ok(&back);
        assert!(back.data["text"].as_str().unwrap().contains("Saved Alice"));
        let redirect = call(
            &registry,
            &ctx,
            "browser.navigate",
            json!({"browser_id":browser_id,"url":format!("{}/redirect",server.origin)}),
        )
        .await
        .unwrap();
        assert!(
            !redirect.data["blocked_requests"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{}",
            redirect.data
        );
        assert!(other.requests.lock().unwrap().is_empty());
        let closed = call(
            &registry,
            &ctx,
            "browser.close",
            json!({"browser_id":browser_id}),
        )
        .await
        .unwrap();
        assert_ok(&closed);
        assert_eq!(
            observe
                .prepare(json!({"browser_id":browser_id}), &ctx)
                .await
                .unwrap_err()
                .code,
            "browser_session_missing"
        );
        // Without explicit private-origin consent even an allowed origin cannot reach localhost.
        let before = server.requests.lock().unwrap().len();
        let denied = call(
            &registry,
            &ctx,
            "browser.open",
            json!({"url":server.origin,"browser":browser}),
        )
        .await
        .unwrap();
        assert_eq!(server.requests.lock().unwrap().len(), before);
        assert_eq!(
            denied.data["blocked_requests"][0]["reason"],
            "network_denied"
        );
        call(
            &registry,
            &ctx,
            "browser.close",
            json!({"browser_id":denied.data["browser_id"]}),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn cancelling_an_action_closes_its_browser_and_preview() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path()).await;
    let server = support::Server::start(|_| support::Response {
        status: 200,
        headers: vec![("content-type".into(), "text/html".into())],
        body: b"<body>Cancel fixture</body>".to_vec(),
    })
    .await;
    let registry = ToolRegistry::builtins().unwrap();
    let opened = call(
        &registry,
        &ctx,
        "browser.open",
        json!({"url":server.origin,"private_origins":[server.origin]}),
    )
    .await
    .unwrap();
    assert_ok(&opened);
    let browser_id = opened.data["browser_id"].clone();
    let viewer = opened.data["viewer_url"].as_str().unwrap().to_owned();
    let tool = registry.get("browser.action").unwrap();
    let args = tool
        .prepare(
            json!({"browser_id":browser_id,"action":"wait","ms":2000}),
            &ctx,
        )
        .await
        .unwrap();
    let cancel = ctx.cancel.clone();
    let cancelling = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel();
    };
    let (result, _) = tokio::join!(tool.execute(args, &ctx), cancelling);
    assert_eq!(result.unwrap_err().code, "outcome_unknown");
    assert_eq!(
        registry
            .get("browser.observe")
            .unwrap()
            .prepare(json!({"browser_id":browser_id}), &ctx)
            .await
            .unwrap_err()
            .code,
        "browser_session_missing"
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    assert!(client.get(viewer).send().await.is_err());
    // A host can lower the dedicated browser budget; the process fails closed
    // with an actionable reason instead of bypassing the resource guard.
    let mut limited = ctx.clone();
    limited.cancel = CancellationToken::new();
    limited.sandbox.limits.browser_session_memory_bytes = 1;
    let result = call(
        &registry,
        &limited,
        "browser.open",
        json!({"url":server.origin,"private_origins":[server.origin]}),
    )
    .await
    .unwrap_err();
    assert_eq!(result.code, "outcome_unknown");
    assert!(
        result.message.contains("memory limit"),
        "{}",
        result.message
    );
}

#[tokio::test]
#[ignore = "explicit network smoke against example.com"]
async fn public_https_through_the_isolated_browser_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path()).await;
    for browser in std::env::var("WW_BROWSER_ENGINES")
        .unwrap_or("chromium".into())
        .split(',')
    {
        let registry = ToolRegistry::builtins().unwrap();
        let opened = call(
            &registry,
            &ctx,
            "browser.open",
            json!({"url":"https://example.com","browser":browser}),
        )
        .await
        .unwrap();
        assert_ok(&opened);
        assert_eq!(opened.data["title"], "Example Domain");
        call(
            &registry,
            &ctx,
            "browser.close",
            json!({"browser_id":opened.data["browser_id"]}),
        )
        .await
        .unwrap();
    }
}
