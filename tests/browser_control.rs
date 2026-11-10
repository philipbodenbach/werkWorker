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

#[tokio::test]
async fn browser_target_recovers_after_output_limit_and_compaction_in_the_engine() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    use werk_worker::{
        browser_policy::BrowserPolicy, profiles::AgentProfile, providers::ModelDelta,
    };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("preview.html"),
        format!(
            "<!doctype html><title>Recovered preview</title><body>Current local page</body>\n{}",
            "<!-- previous source evidence -->\n".repeat(400)
        ),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("notes.txt"),
        "Historical inspection evidence.\n".repeat(250),
    )
    .unwrap();
    let call = |id: &str, name: &str, arguments: Value| ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::calls(
            "",
            vec![call("read", "files.read", json!({"path":"preview.html"}))],
        ),
        ScriptedTurn::calls(
            "",
            vec![call("notes", "files.read", json!({"path":"notes.txt"}))],
        ),
        ScriptedTurn {
            delay_ms: 0,
            error: None,
            deltas: vec![
                ModelDelta::Tool {
                    index: 0,
                    id: "incomplete".into(),
                    name: "browser.open".into(),
                    arguments: "{\"path\":".into(),
                },
                ModelDelta::Finish {
                    reason: "length".into(),
                },
                ModelDelta::Done,
            ],
        },
        ScriptedTurn::calls("", vec![call("missing_target", "browser.open", json!({}))]),
        ScriptedTurn::calls(
            "",
            vec![call(
                "corrected_target",
                "browser.open",
                json!({"path":"preview.html"}),
            )],
        ),
        ScriptedTurn::text("Opened the local preview."),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let mut browser = BrowserPolicy::default();
    browser
        .set(
            "http://workspace.invalid",
            "browsing",
            ToolPermission::Allow,
        )
        .unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "preview".into(),
                prompt: "Inspect and preview the local file.".into(),
                tools: vec!["files.read".into(), "browser.open".into()],
                skills: vec![],
            },
            dir.path(),
            HostPolicy {
                browser: Some(browser),
                tool_permissions: [("files.read".into(), ToolPermission::Allow)].into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Open preview.html in the browser",
            "start",
            Limits {
                // The v3 schema and bundled browser skill add context. The
                // large read fixtures below still require actual compaction.
                context_tokens: 20480,
                reserve_output_tokens: 1024,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(60), run.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{result:?}");
    let saved = run.status().await.unwrap();
    assert_eq!(saved.metrics.model_requests, 6);
    assert_eq!(
        saved.invocations.len(),
        4,
        "incomplete calls must never enter execution"
    );
    assert_eq!(saved.invocations[2].call.arguments, json!({}));
    assert_eq!(
        saved.invocations[2].result.as_ref().unwrap().status,
        Outcome::ValidationError
    );
    let opened = saved.invocations[3].result.as_ref().unwrap();
    assert_ok(opened);
    assert!(
        opened.data["text"]
            .as_str()
            .unwrap()
            .contains("Current local page")
    );
    assert!(
        opened.data["viewer_url"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:")
    );
    assert!(
        saved
            .contexts
            .iter()
            .any(|c| !c.omitted_messages.is_empty()),
        "exercise compacted context"
    );
    let requests = provider.requests();
    let correction = &requests[4];
    let diagnostic = correction
        .messages
        .iter()
        .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("missing_target"))
        .unwrap();
    assert!(diagnostic.content.contains("exactly one alternative"));
    assert!(diagnostic.content.contains("path"));
    assert!(diagnostic.content.contains("url"));
    assert!(
        correction
            .messages
            .iter()
            .any(|m| m.content.contains("preview.html"))
    );
    werk_worker::context::validate_messages(&session.snapshot().await.unwrap().messages).unwrap();
    engine.close().await.unwrap();
}

#[tokio::test]
async fn workspace_open_is_scoped_validated_and_bound_to_the_reviewed_entry() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<body>Before</body>").unwrap();
    let ctx = context(dir.path()).await;
    let registry = ToolRegistry::builtins().unwrap();
    let tool = registry.get("browser.open").unwrap();
    for args in [
        json!({}),
        json!({"path":"index.html","url":"https://example.com"}),
    ] {
        let error = validate_arguments(&tool.definition().input_schema, &args).unwrap_err();
        assert!(error.message.contains("exactly one alternative"));
        assert!(error.message.contains("required fields [\"path\"]"));
        assert!(error.message.contains("required fields [\"url\"]"));
    }
    let prepared = tool
        .prepare(json!({"path":"index.html"}), &ctx)
        .await
        .unwrap();
    assert_eq!(prepared["url"], "http://workspace.invalid/index.html");
    assert_eq!(prepared["private_origins"], json!([]));
    assert_eq!(prepared["workspace_path"], "index.html");
    let policy = HostPolicy {
        browser: Some(Default::default()),
        ..Default::default()
    };
    assert_eq!(
        policy.evaluate(&tool.definition(), &prepared, &[]),
        PolicyDecision::Ask
    );
    let mut denied = policy.clone();
    denied
        .browser
        .as_mut()
        .unwrap()
        .set("http://workspace.invalid", "browsing", ToolPermission::Deny)
        .unwrap();
    assert_eq!(
        denied.evaluate(&tool.definition(), &prepared, &[]),
        PolicyDecision::Deny
    );
    std::fs::write(dir.path().join("index.html"), "<body>Changed</body>").unwrap();
    assert_eq!(
        tool.execute(prepared, &ctx).await.unwrap_err().code,
        "write_conflict"
    );
    assert!(
        tool.prepare(json!({"path":"missing.html"}), &ctx)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn real_workspace_browser_loads_assets_reuses_session_and_reloads_edits() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("site")).unwrap();
    let html = r#"<!doctype html><title>Local</title><link rel="stylesheet" href="style.css"><body><button id="next">Next</button><output id="state">Waiting</output><img id="icon" src="icon.svg"><script type="module" src="app.js"></script></body>"#;
    std::fs::write(dir.path().join("site/index.html"), html).unwrap();
    std::fs::write(
        dir.path().join("site/style.css"),
        "#state { color: rgb(20, 40, 60) }",
    )
    .unwrap();
    std::fs::write(dir.path().join("site/icon.svg"), r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="red"/></svg>"#).unwrap();
    std::fs::write(
        dir.path().join("site/value.js"),
        "export const value = 'Ready';",
    )
    .unwrap();
    std::fs::write(dir.path().join("site/app.js"), "import {value} from './value.js'; document.querySelector('#state').textContent=value; document.querySelector('#next').onclick=()=>document.querySelector('#state').textContent='Clicked';").unwrap();
    std::fs::write(
        dir.path().join("site/second.html"),
        "<body>Second page</body>",
    )
    .unwrap();
    let ctx = context(dir.path()).await;
    let registry = ToolRegistry::builtins().unwrap();
    let opened = call(
        &registry,
        &ctx,
        "browser.open",
        json!({"path":"site/index.html"}),
    )
    .await
    .unwrap();
    assert_ok(&opened);
    assert!(
        opened.data["text"].as_str().unwrap().contains("Ready"),
        "{}",
        opened.data
    );
    assert_eq!(opened.data["reused_session"], false);
    let browser_id = opened.data["browser_id"].clone();
    let inspected = call(&registry, &ctx, "browser.evaluate", json!({"browser_id":browser_id,"script":"() => ({color:getComputedStyle(document.querySelector('#state')).color,image:document.querySelector('#icon').naturalWidth, value:document.querySelector('#state').textContent})"})).await.unwrap();
    assert_ok(&inspected);
    assert_eq!(
        inspected.data["script_result"],
        json!({"color":"rgb(20, 40, 60)","image":10,"value":"Ready"})
    );
    let clicked = call(&registry, &ctx, "browser.action", json!({"browser_id":browser_id,"action":"click","target":{"selector":"#next"},"screenshot":true})).await.unwrap();
    assert_ok(&clicked);
    assert!(clicked.data["text"].as_str().unwrap().contains("Clicked"));
    assert_eq!(clicked.images.len(), 1);
    let frame = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!(
            "{}frame",
            opened.data["viewer_url"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(image::load_from_memory(&frame).is_ok());
    std::fs::write(
        dir.path().join("site/value.js"),
        "export const value = 'Updated';",
    )
    .unwrap();
    let reloaded = call(
        &registry,
        &ctx,
        "browser.action",
        json!({"browser_id":browser_id,"action":"reload"}),
    )
    .await
    .unwrap();
    assert_ok(&reloaded);
    assert!(reloaded.data["text"].as_str().unwrap().contains("Updated"));
    // Repeated opens are navigation, including the formerly rejected file URL.
    let reopened = call(
        &registry,
        &ctx,
        "browser.open",
        json!({"url":"file:///workspace/site/second.html"}),
    )
    .await
    .unwrap();
    assert_ok(&reopened);
    assert_eq!(reopened.data["browser_id"], browser_id);
    assert_eq!(reopened.data["viewer_url"], opened.data["viewer_url"]);
    assert_eq!(reopened.data["reused_session"], true);
    assert!(
        reopened.data["text"]
            .as_str()
            .unwrap()
            .contains("Second page")
    );
    let returned = call(
        &registry,
        &ctx,
        "browser.navigate",
        json!({"browser_id":browser_id,"url":"file:///workspace/site/index.html"}),
    )
    .await
    .unwrap();
    assert_ok(&returned);
    assert!(returned.data["text"].as_str().unwrap().contains("Updated"));
    // A fresh browser can also start from the file alias.
    assert_ok(
        &call(
            &registry,
            &ctx,
            "browser.close",
            json!({"browser_id":browser_id}),
        )
        .await
        .unwrap(),
    );
    let fresh = call(
        &registry,
        &ctx,
        "browser.open",
        json!({"url":"file:///workspace/site/index.html"}),
    )
    .await
    .unwrap();
    assert_ok(&fresh);
    assert_ne!(fresh.data["browser_id"], browser_id);
    assert_ok(
        &call(
            &registry,
            &ctx,
            "browser.close",
            json!({"browser_id":fresh.data["browser_id"]}),
        )
        .await
        .unwrap(),
    );
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
    let err = action
        .prepare(json!({"browser_id":"x","action":"type"}), &ctx)
        .await
        .unwrap_err();
    assert_eq!(err.code, "invalid_browser_action");
    let err = action
        .prepare(
            json!({"browser_id":"x","action":"type","text":"hello","target":{"selector":"input"}}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "invalid_browser_target");
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
    if let Err(e) = ctx.sandbox.require_process_backend().await {
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
                let name = r.body["name"].as_str().unwrap();
                assert!(matches!(name, "Alice" | "Grüße 世界"));
                assert!(r.raw.contains("fixture=session"));
                (200, format!("Saved {name}"), vec![])
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
        // Focus, replace text and submit entirely through mouse/keyboard input.
        for args in [
            json!({"action":"move","x":100,"y":18}),
            json!({"action":"click_at","x":100,"y":18}),
            json!({"action":"press","key":"Control+a"}),
            json!({"action":"type","text":"Grüße 世界"}),
            json!({"action":"press","key":"Tab"}),
        ] {
            let mut args = args;
            args["browser_id"] = browser_id.clone();
            assert_ok(&call(&registry, &ctx, "browser.action", args).await.unwrap());
        }
        let keyboard_save = call(
            &registry,
            &ctx,
            "browser.action",
            json!({"browser_id":browser_id,"action":"press","key":"Enter"}),
        )
        .await
        .unwrap();
        assert_ok(&keyboard_save);
        assert!(
            keyboard_save.data["text"]
                .as_str()
                .unwrap()
                .contains("Saved Grüße 世界"),
            "{}",
            keyboard_save.data
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
                .contains("Read-only preview")
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
        let expansion = nav
            .prepare(json!({"browser_id":browser_id,"url":other.origin}), &ctx)
            .await
            .unwrap();
        assert_eq!(expansion["new_origins"], json!([other.origin]));
        let browser_policy = werk_worker::browser_policy::BrowserPolicy::default();
        assert_eq!(
            browser_policy.evaluate("browser.navigate", &expansion),
            PolicyDecision::Ask
        );
        assert!(other.requests.lock().unwrap().is_empty());
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
        assert!(
            back.data["text"]
                .as_str()
                .unwrap()
                .contains("Saved Grüße 世界")
        );
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

#[tokio::test]
async fn downloads_keep_cookie_scope_and_produce_owned_artifacts() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let server = support::Server::start(|r| {
        let download = r.path == "/download";
        if download {
            assert!(r.raw.contains("download=session"));
        }
        support::Response {
            status: 200,
            headers: vec![
                (
                    "content-type".into(),
                    if download {
                        "application/octet-stream"
                    } else {
                        "text/html"
                    }
                    .into(),
                ),
                (
                    "set-cookie".into(),
                    "download=session; Path=/; HttpOnly".into(),
                ),
            ],
            body: if download {
                b"owned download bytes".to_vec()
            } else {
                b"<title>Download fixture</title><body>Ready</body>".to_vec()
            },
        }
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let mut ctx = context(dir.path()).await;
    ctx.artifacts = Some(ArtifactStore::open(artifacts.path()).await.unwrap());
    for browser in std::env::var("WW_BROWSER_ENGINES")
        .unwrap_or("chromium".into())
        .split(',')
    {
        let registry = ToolRegistry::builtins().unwrap();
        let opened = call(
            &registry,
            &ctx,
            "browser.open",
            json!({"url":server.origin,"private_origins":[server.origin],"browser":browser}),
        )
        .await
        .unwrap();
        assert_ok(&opened);
        let browser_id = opened.data["browser_id"].clone();
        let result = call(
            &registry,
            &ctx,
            "browser.download",
            json!({"browser_id":browser_id,"url":format!("{}/download",server.origin)}),
        )
        .await
        .unwrap();
        assert_ok(&result);
        assert_eq!(result.artifacts.len(), 1);
        let artifact = &result.artifacts[0];
        assert_eq!(artifact.session_id, ctx.session_id);
        assert_eq!(artifact.run_id, ctx.run_id);
        assert_eq!(
            ctx.artifacts
                .as_ref()
                .unwrap()
                .read(artifact, &ctx.session_id, 0, 1024)
                .await
                .unwrap(),
            b"owned download bytes"
        );
        let history = call(
            &registry,
            &ctx,
            "browser.history",
            json!({"browser_id":browser_id}),
        )
        .await
        .unwrap();
        assert_ok(&history);
        assert!(
            history.data["history"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["url"].as_str().unwrap().starts_with(&server.origin))
        );
        call(
            &registry,
            &ctx,
            "browser.close",
            json!({"browser_id":browser_id}),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn module_heavy_pages_load_with_a_bounded_request_budget() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for index in 0..300 {
        std::fs::write(
            dir.path().join(format!("part-{index}.js")),
            "window.loaded=(window.loaded||0)+1;",
        )
        .unwrap();
    }
    for count in [128, 300] {
        let scripts: String = (0..count)
            .map(|i| format!("<script src='part-{i}.js'></script>"))
            .collect();
        std::fs::write(dir.path().join("index.html"), format!("<!doctype html><body>{scripts}<script>document.body.textContent='Modules loaded: '+window.loaded</script>")).unwrap();
        let ctx = context(dir.path()).await;
        let registry = ToolRegistry::builtins().unwrap();
        let result = call(
            &registry,
            &ctx,
            "browser.open",
            json!({"path":"index.html","screenshot":true}),
        )
        .await
        .unwrap();
        call(
            &registry,
            &ctx,
            "browser.close",
            json!({"browser_id":result.data["browser_id"]}),
        )
        .await
        .unwrap();
        assert_ok(&result);
        assert!(!result.images.is_empty());
        if count == 128 {
            assert_eq!(result.data["text"], "Modules loaded: 128");
            assert!(
                result.data["network"]["blocked"]
                    .as_object()
                    .unwrap()
                    .is_empty()
            );
        } else {
            assert!(
                result.data["network"]["blocked"]["browser_network_limit"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            assert_ne!(result.data["text"], "Modules loaded: 300");
        }
    }
}

#[tokio::test]
#[ignore = "explicit live-site compatibility probe; no model decisions"]
async fn live_jspaint_observations_remain_available() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path()).await;
    let registry = ToolRegistry::builtins().unwrap();
    let opened = call(
        &registry,
        &ctx,
        "browser.open",
        json!({"url":"https://jspaint.app/", "screenshot":true}),
    )
    .await
    .unwrap();
    let browser_id = opened.data["browser_id"].clone();
    let mut results = vec![opened];
    let editor_ready = |r: &ToolResult| {
        r.data["visual_surface"] == true
            && r.data["title"]
                .as_str()
                .is_some_and(|title| title.ends_with(" - Paint"))
    };
    for _ in 0..5 {
        results.push(
            call(
                &registry,
                &ctx,
                "browser.observe",
                json!({"browser_id":browser_id,"screenshot":true}),
            )
            .await
            .unwrap(),
        );
        if results.len() >= 3 && results.iter().rev().take(2).all(editor_ready) {
            break;
        }
    }
    call(
        &registry,
        &ctx,
        "browser.close",
        json!({"browser_id":browser_id}),
    )
    .await
    .unwrap();
    if let Ok(path) = std::env::var("WW_JSPAINT_FRAME")
        && let Some(image) = results.last().and_then(|r| r.images.last())
    {
        use base64::Engine;
        std::fs::write(
            path,
            base64::engine::general_purpose::STANDARD
                .decode(&image.data_base64)
                .unwrap(),
        )
        .unwrap();
    }
    for result in &results {
        println!(
            "{}",
            json!({"status":result.status,"error":result.data["error"],"errors":result.data["observation_errors"],"transport":result.data["transport_diagnostics"],"network":result.data["network"],"visual_surface":result.data["visual_surface"],"title":result.data["title"]})
        );
    }
    for result in results.iter().rev().take(2) {
        assert!(
            editor_ready(result),
            "Editor did not become usable: {}",
            result.data
        );
    }
    for (index, result) in results.into_iter().enumerate() {
        // A remote navigation deadline can expire while the editor still loads.
        // Only tolerate that first timeout if the subsequent observations prove
        // the editor loaded and remained usable without reopening the page.
        if index == 0 && result.status == Outcome::Failed {
            assert!(
                result.data["error"]
                    .as_str()
                    .is_some_and(|e| e.starts_with("Page.goto: Timeout")),
                "{}",
                result.data
            );
        } else {
            assert_eq!(result.status, Outcome::Succeeded, "{}", result.data);
        }
        assert!(!result.images.is_empty(), "{}", result.data);
        assert!(result.data["network"]["blocked"]["browser_network_limit"].is_null());
    }
}

#[tokio::test]
#[ignore = "explicit JSPaint mouse-gesture probe; draws only through browser.action"]
async fn live_jspaint_rectangle_uses_opposite_corner_drag() {
    use base64::Engine;
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path()).await;
    let registry = ToolRegistry::builtins().unwrap();
    let opened = call(
        &registry,
        &ctx,
        "browser.open",
        json!({"url":"https://jspaint.app/", "screenshot":true}),
    )
    .await
    .unwrap();
    let browser_id = opened.data["browser_id"].clone();
    for _ in 0..3 {
        let observed = call(
            &registry,
            &ctx,
            "browser.observe",
            json!({"browser_id":browser_id,"screenshot":true}),
        )
        .await
        .unwrap();
        assert_ok(&observed);
    }
    let selected = call(
        &registry,
        &ctx,
        "browser.action",
        json!({"browser_id":browser_id,"action":"click_at","x":15,"y":181,"screenshot":true}),
    )
    .await
    .unwrap();
    let closed = call(&registry, &ctx, "browser.action", json!({"browser_id":browser_id,"action":"drag","path":[{"x":200,"y":400},{"x":450,"y":400},{"x":450,"y":550},{"x":200,"y":550},{"x":200,"y":400}],"screenshot":true})).await.unwrap();
    let closed_inside = call(&registry, &ctx, "browser.action", json!({"browser_id":browser_id,"action":"drag","path":[{"x":200,"y":150},{"x":450,"y":150},{"x":450,"y":300},{"x":200,"y":300},{"x":200,"y":150}],"screenshot":true})).await.unwrap();
    let diagonal = call(&registry, &ctx, "browser.action", json!({"browser_id":browser_id,"action":"drag","path":[{"x":200,"y":150},{"x":450,"y":300}],"screenshot":true})).await.unwrap();
    call(
        &registry,
        &ctx,
        "browser.close",
        json!({"browser_id":browser_id}),
    )
    .await
    .unwrap();
    for result in [&selected, &closed, &closed_inside, &diagonal] {
        assert_ok(result);
    }
    let decode = |result: &ToolResult| {
        let image = result.images.first().expect("current screenshot");
        assert_eq!((image.width, image.height), (1024, 768));
        base64::engine::general_purpose::STANDARD
            .decode(&image.data_base64)
            .unwrap()
    };
    let dark_pixels = |result: &ToolResult| {
        let pixels = image::load_from_memory(&decode(result)).unwrap().to_rgb8();
        (145..306)
            .flat_map(|y| (195..456).map(move |x| (x, y)))
            .filter(|&(x, y)| pixels.get_pixel(x, y).0.iter().any(|v| *v < 160))
            .count()
    };
    let before = dark_pixels(&selected);
    let closed_count = dark_pixels(&closed);
    let closed_inside_count = dark_pixels(&closed_inside);
    let diagonal_count = dark_pixels(&diagonal);
    eprintln!(
        "JSPaint canvas dark pixels: before={before}, original_path={closed_count}, closed_inside={closed_inside_count}, opposite_corners={diagonal_count}"
    );
    if let Ok(path) = std::env::var("WW_JSPAINT_FRAME") {
        std::fs::write(path, decode(&diagonal)).unwrap();
    }
    assert!(before < 20, "canvas starts blank");
    assert!(
        closed_count < 80,
        "original path does not draw inside the target area"
    );
    assert!(
        closed_inside_count < 80,
        "returning to the first corner collapses the shape"
    );
    assert!(
        diagonal_count > 500,
        "opposite corners must leave a visible rectangle"
    );
}

#[tokio::test]
#[ignore = "explicit live-site compatibility probe; no model decisions"]
async fn live_browser_compatibility_probe() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path()).await;
    let registry = ToolRegistry::builtins().unwrap();
    let mut reports = Vec::new();
    for url in [
        "https://jspaint.app/",
        "https://www.nytimes.com/games/wordle/index.html",
        "https://lichess.org/",
        "https://www.crazygames.com/game/8-ball-pool-billiards-multiplayer",
    ] {
        let result = call(&registry, &ctx, "browser.open", json!({"url":url})).await;
        match result {
            Ok(r) => {
                reports.push(json!({"url":url,"status":r.status,"error_code":r.error_code,"action_error":r.data["error"],"blocked_requests":r.data["blocked_requests"],"transports":r.data["transport_diagnostics"],"title":r.data["title"],"controls":r.data["controls"].as_array().map(Vec::len),"images":r.images.len()}));
                if r.data["browser_id"].is_string() {
                    let _ = call(
                        &registry,
                        &ctx,
                        "browser.close",
                        json!({"browser_id":r.data["browser_id"]}),
                    )
                    .await;
                }
            }
            Err(e) => reports.push(json!({"url":url,"error_code":e.code})),
        }
    }
    let report = serde_json::to_string_pretty(&reports).unwrap();
    if let Ok(path) = std::env::var("WW_COMPAT_REPORT") {
        std::fs::write(path, &report).unwrap();
    }
    println!("{report}");
}

#[tokio::test]
async fn host_browser_lifetime_idle_and_action_budgets_are_enforced() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    // Leave startup headroom on hosts also running a local inference backend.
    for (lifetime, idle, actions) in [(6, 0, 400), (0, 6, 400), (0, 0, 1), (0, 0, 400)] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("page.html"), "<body>Budget</body>").unwrap();
        let ctx = context(dir.path()).await;
        let registry = ToolRegistry::builtins().unwrap();
        let tool = registry.get("browser.open").unwrap();
        let mut args = tool
            .prepare(json!({"path":"page.html"}), &ctx)
            .await
            .unwrap();
        args["_browser_settings"] = json!({"perception":"dom","lifetime_seconds":lifetime,"idle_seconds":idle,"max_actions":actions});
        let opened = tool.execute(args, &ctx).await.unwrap();
        assert_ok(&opened);
        if actions > 1 {
            tokio::time::sleep(Duration::from_secs(7)).await;
        }
        let result = call(
            &registry,
            &ctx,
            "browser.observe",
            json!({"browser_id":opened.data["browser_id"]}),
        )
        .await;
        if lifetime == 0 && idle == 0 && actions > 1 {
            assert_ok(&result.unwrap());
            assert_ok(
                &call(
                    &registry,
                    &ctx,
                    "browser.close",
                    json!({"browser_id":opened.data["browser_id"]}),
                )
                .await
                .unwrap(),
            );
            assert!(
                call(
                    &registry,
                    &ctx,
                    "browser.observe",
                    json!({"browser_id":opened.data["browser_id"]})
                )
                .await
                .is_err()
            );
            continue;
        }
        let error = result.unwrap_err();
        assert!(
            matches!(
                error.code.as_str(),
                "browser_action_limit" | "outcome_unknown"
            ),
            "{error:?}"
        );
        if actions == 1 {
            // Reaching the action limit blocks further actions but leaves the
            // browser available for explicit cleanup and manual control.
            assert_ok(
                &call(
                    &registry,
                    &ctx,
                    "browser.close",
                    json!({"browser_id":opened.data["browser_id"]}),
                )
                .await
                .unwrap(),
            );
        }
    }
}

#[tokio::test]
async fn browser_images_reach_the_actual_werk_backend_request_in_vision_and_dom_modes() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    use werk_worker::{browser_policy::*, profiles::AgentProfile, providers::*};
    for perception in [Perception::Vision, Perception::Dom] {
        let step = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = step.clone();
        let server=support::Server::start(move |request| {
            if request.path!="/v1/chat/completions" {return support::werk(request);}
            let n=counter.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            let open = request.body["tools"].as_array().unwrap().iter()
                .find(|t|t["function"]["name"]=="browser.open").unwrap();
            assert!(open["function"]["parameters"].get("oneOf").is_none());
            for branch in open["function"]["parameters"]["allOf"][0]["oneOf"].as_array().unwrap() {
                let target=branch["required"][0].as_str().unwrap();
                assert_eq!(branch["type"],"object");
                assert_eq!(branch["properties"][target]["type"],"string");
                assert!(validate_arguments(branch,&json!({})).is_err());
            }
            let messages=request.body["messages"].as_array().unwrap();
            let images=messages.iter().filter_map(|m|m["content"].as_array()).flatten().filter(|p|p["type"]=="image_url").count();
            if perception==Perception::Vision && n>0 { assert!((1..=2).contains(&images),"current image must survive, comparison frame is optional"); }
            else {assert_eq!(images,0);}
            for part in messages.iter().filter_map(|m|m["content"].as_array()).flatten().filter(|p|p["type"]=="image_url") {
                assert!(part["image_url"]["url"].as_str().unwrap().starts_with("data:image/jpeg;base64,"));
            }
            let browser_id=messages.iter().rev().filter(|m|m["role"]=="tool").find_map(|m| {
                let data:Value=serde_json::from_str(m["content"].as_str()?).ok()?;data["data"]["browser_id"].as_str().map(str::to_owned)
            });
            let (name,args)=match n {
                0=>("browser.open",json!({"path":"page.html"})),
                1=>("browser.navigate",json!({"browser_id":browser_id,"url":"http://workspace.invalid/page.html"})),
                2=>("browser.action",json!({"browser_id":browser_id,"action":"click","target":{"selector":"button"}})),
                _=>return support::werk(request),
            };
            support::Response::json(json!({"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":format!("call{n}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]}))
        }).await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("page.html"),
            "<button onclick='this.textContent=\"Done\"'>Change</button>",
        )
        .unwrap();
        let config = WerkConfig {
            base_url: server.origin.clone(),
            stream: false,
            capabilities: ModelCapabilities {
                tool_calling: Support::Supported,
                context_tokens: Some(32768),
                images: if perception == Perception::Dom {
                    Support::Unsupported
                } else {
                    Support::Unknown
                },
                ..Default::default()
            },
            ..WerkConfig::local("fixture-model")
        };
        let provider = WerkProvider::connect(config, None).await.unwrap();
        let engine = WorkerEngine::builder(Arc::new(provider))
            .build()
            .await
            .unwrap();
        let mut browser = BrowserPolicy {
            settings: BrowserSettings {
                perception,
                ..Default::default()
            },
            ..Default::default()
        };
        for category in ["browsing", "actions"] {
            browser
                .set("http://workspace.invalid", category, ToolPermission::Allow)
                .unwrap();
        }
        let session = engine
            .create_session(
                AgentProfile {
                    schema_version: 1,
                    name: "wire".into(),
                    prompt: "Inspect".into(),
                    tools: ["browser.open", "browser.navigate", "browser.action"]
                        .map(str::to_owned)
                        .into(),
                    skills: vec![],
                },
                dir.path(),
                HostPolicy {
                    browser: Some(browser),
                    ..Default::default()
                },
                Value::Null,
            )
            .await
            .unwrap();
        let run = session
            .submit(
                "Open, navigate, click and verify",
                "wire",
                Limits::default(),
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.status, RunStatus::Completed, "{result:?}");
        assert_eq!(step.load(std::sync::atomic::Ordering::SeqCst), 4);
        let state = run.status().await.unwrap();
        assert!(
            state
                .invocations
                .last()
                .unwrap()
                .result
                .as_ref()
                .unwrap()
                .data["text"]
                .as_str()
                .unwrap()
                .contains("Done")
        );
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn scaled_canvas_coordinates_and_explicit_perception_overrides() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("noise.html"),r#"<style>body{margin:0}</style><canvas width="1024" height="768"></canvas><script>
    const c=document.querySelector('canvas'),g=c.getContext('2d'),d=g.createImageData(1024,768);let seed=17;
    for(let i=0;i<d.data.length;i+=4){for(let j=0;j<3;j++){seed=(Math.imul(seed,1664525)+1013904223)|0;d.data[i+j]=(seed>>>24);}d.data[i+3]=255;}g.putImageData(d,0,0);
    c.onclick=e=>{g.fillStyle='#ff0000';g.fillRect(e.clientX-10,e.clientY-10,20,20);};</script>"#).unwrap();
    let ctx = context(dir.path()).await;
    let registry = ToolRegistry::builtins().unwrap();
    let opened = call(
        &registry,
        &ctx,
        "browser.open",
        json!({"path":"noise.html","visual":true}),
    )
    .await
    .unwrap();
    assert_ok(&opened);
    let image = &opened.images[0];
    assert!(image.width < 1024, "noise must exercise actual resizing");
    let x = (160.0 * f64::from(image.width) / 1024.0
        * opened.data["screenshot"]["scale_x"].as_f64().unwrap())
    .round() as u64;
    let y = (220.0 * f64::from(image.height) / 768.0
        * opened.data["screenshot"]["scale_y"].as_f64().unwrap())
    .round() as u64;
    let browser_id = opened.data["browser_id"].clone();
    let clicked = call(
        &registry,
        &ctx,
        "browser.action",
        json!({"browser_id":browser_id,"action":"click_at","x":x,"y":y,"screenshot":false}),
    )
    .await
    .unwrap();
    assert_ok(&clicked);
    assert_eq!(
        clicked.images.len(),
        1,
        "Auto must retain feedback on a detected drawing surface even if the model requests screenshot:false"
    );
    assert_eq!(clicked.data["action_target"]["tag"], "canvas");
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let bytes = client
        .get(format!(
            "{}frame",
            opened.data["viewer_url"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let pixels = image::load_from_memory(&bytes).unwrap().to_rgb8();
    let pixel = pixels.get_pixel(160, 220);
    assert!(
        pixel[0] > 220 && pixel[1] < 40 && pixel[2] < 40,
        "{pixel:?}"
    );
    let tool = registry.get("browser.observe").unwrap();
    for (perception, screenshot, frames) in
        [("dom", true, 1), ("dom", false, 0), ("vision", false, 1)]
    {
        let mut args = tool
            .prepare(
                json!({"browser_id":browser_id,"screenshot":screenshot}),
                &ctx,
            )
            .await
            .unwrap();
        args["_browser_settings"] = json!({"perception":perception});
        assert_eq!(
            tool.execute(args, &ctx).await.unwrap().images.len(),
            frames,
            "{perception} with screenshot:{screenshot} must retain the host's image policy"
        );
    }
    let mut args = tool
        .prepare(json!({"browser_id":browser_id,"screenshot":false}), &ctx)
        .await
        .unwrap();
    args["_browser_settings"] = json!({"perception":"vision"});
    args["_images_supported"] = json!(false);
    assert_eq!(
        tool.execute(args, &ctx).await.unwrap_err().code,
        "images_unsupported"
    );
    let mut denied = tool
        .prepare(json!({"browser_id":browser_id,"screenshot":true}), &ctx)
        .await
        .unwrap();
    denied["_images_supported"] = json!(false);
    assert_eq!(
        tool.execute(denied, &ctx).await.unwrap_err().code,
        "images_unsupported"
    );
    let close = registry.get("browser.close").unwrap();
    let mut close_args = close
        .prepare(json!({"browser_id":browser_id}), &ctx)
        .await
        .unwrap();
    close_args["_browser_settings"] = json!({"perception":"vision"});
    close_args["_images_supported"] = json!(false);
    assert_ok(&close.execute(close_args, &ctx).await.unwrap());
}
