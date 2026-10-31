#![cfg(target_os = "linux")]
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use werk_worker::{runtime::*, tools::*, *};

async fn context(dir: &std::path::Path) -> ToolContext {
    ToolContext {
        workspace: Workspace::open(dir).await.unwrap(),
        workspace_lease: None,
        session_id: "session_test".into(),
        run_id: "run_test".into(),
        invocation_id: "invocation_test".into(),
        writable: true,
        cancel: CancellationToken::new(),
        sandbox: Sandbox::default(),
        jobs: Arc::new(JobManager::default()),
        artifacts: None,
        artifact_refs: vec![],
        skills: Default::default(),
        native_jobs: vec![],
    }
}
async fn call(ctx: &ToolContext, name: &str, args: Value) -> Result<ToolResult> {
    let tool = ToolRegistry::builtins()?.get(name)?;
    validate_arguments(&tool.definition().input_schema, &args)?;
    tool.execute(tool.prepare(args, ctx).await?, ctx).await
}
#[tokio::test]
async fn recursive_discovery_filters_pages_and_single_file_search() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
    std::fs::create_dir_all(dir.path().join("node_modules/lib")).unwrap();
    std::fs::write(dir.path().join("node_modules/lib/ignore.js"), "rotate").unwrap();
    std::fs::write(
        dir.path().join("src/a.js"),
        "before\nrotate(1)\nafter\nrotate(2)\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("src/nested/b.js"), "ROTATE(3)\n").unwrap();
    std::fs::write(dir.path().join("src/nested/c.txt"), "rotate(4)\n").unwrap();
    std::os::unix::fs::symlink("/etc", dir.path().join("escape")).unwrap();
    let ctx = context(dir.path()).await;
    let first = call(
        &ctx,
        "files.find",
        json!({"path":".","pattern":"**/*.js","limit":1}),
    )
    .await
    .unwrap();
    assert_eq!(first.data["paths"], json!(["src/a.js"]));
    assert_eq!(first.data["next_offset"], 1);
    let second = call(
        &ctx,
        "files.find",
        json!({"path":".","pattern":"**/*.js","limit":1,"offset":1}),
    )
    .await
    .unwrap();
    assert_eq!(second.data["paths"], json!(["src/nested/b.js"]));
    assert_eq!(second.data["has_more"], false);
    let search=call(&ctx,"files.search",json!({"path":"src","query":"rotate\\([0-9]","regex":true,"case_sensitive":false,"include":"**/*.js","limit":2})).await.unwrap();
    assert_eq!(search.data["matches"].as_array().unwrap().len(), 2);
    assert_eq!(search.data["matches"][0]["line"], 2);
    assert_eq!(search.data["matches"][0]["context"][0]["text"], "before");
    let next=call(&ctx,"files.search",json!({"path":"src","query":"rotate\\([0-9]","regex":true,"case_sensitive":false,"include":"**/*.js","offset":2})).await.unwrap();
    assert_eq!(next.data["matches"][0]["path"], "src/nested/b.js");
    let single = call(
        &ctx,
        "files.search",
        json!({"path":"src/a.js","query":"rotate(2)"}),
    )
    .await
    .unwrap();
    assert_eq!(single.data["matches"][0]["line"], 4);
    assert_eq!(
        call(
            &ctx,
            "files.search",
            json!({"path":"src","query":"[","regex":true})
        )
        .await
        .unwrap_err()
        .code,
        "invalid_pattern"
    );
    assert!(
        call(&ctx, "files.find", json!({"path":"escape","pattern":"**"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn local_image_is_normalized_and_not_duplicated_as_base64_text() {
    let dir = tempfile::tempdir().unwrap();
    image::RgbImage::from_pixel(1500, 1200, image::Rgb([100, 120, 140]))
        .save(dir.path().join("image.png"))
        .unwrap();
    let ctx = context(dir.path()).await;
    let result = call(&ctx, "files.read_image", json!({"path":"image.png"}))
        .await
        .unwrap();
    assert_eq!(result.images.len(), 1);
    result.images[0].validate().unwrap();
    assert!(result.images[0].width <= 1024);
    let message = Message::tool("call_1", &result);
    assert_eq!(message.images, result.images);
    assert!(!message.content.contains("data_base64"));
    let legacy: ToolResult = serde_json::from_value(
        json!({"status":"succeeded","data":{},"error_code":null,"artifacts":[],"truncated":false}),
    )
    .unwrap();
    assert!(legacy.images.is_empty());
}

#[tokio::test]
async fn live_job_logs_wait_and_cancellation_are_run_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path()).await;
    if let Err(error) = ctx.sandbox.doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let job = ctx
        .jobs
        .start(
            &ctx.run_id,
            ctx.sandbox.clone(),
            ctx.workspace.clone(),
            false,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf 'ready\\n'; sleep 1; printf 'done\\n'".into(),
            ],
            ".".into(),
        )
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let log = ctx
            .jobs
            .log(&ctx.run_id, &job, "stdout", Some(0), 4096)
            .await
            .unwrap();
        if log["text"].as_str().unwrap().contains("ready") {
            assert_eq!(
                ctx.jobs.status(&ctx.run_id, &job, false).await.unwrap()["status"],
                "running"
            );
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        ctx.jobs
            .log("other_run", &job, "stdout", None, 100)
            .await
            .is_err()
    );
    let done = ctx
        .jobs
        .wait(&ctx.run_id, &job, 3000, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(done["output"]["exit_code"], 0);
    assert!(done["output"]["stdout"].is_null());
    assert!(
        ctx.jobs
            .log(&ctx.run_id, &job, "stdout", None, 4096)
            .await
            .unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("done")
    );
    let unicode = ctx
        .jobs
        .start(
            &ctx.run_id,
            ctx.sandbox.clone(),
            ctx.workspace.clone(),
            false,
            vec![
                "/usr/bin/python3".into(),
                "-c".into(),
                "print('ä'*35000)".into(),
            ],
            ".".into(),
        )
        .await
        .unwrap();
    ctx.jobs
        .wait(&ctx.run_id, &unicode, 5000, CancellationToken::new())
        .await
        .unwrap();
    let page = ctx
        .jobs
        .log(&ctx.run_id, &unicode, "stdout", Some(0), 1)
        .await
        .unwrap();
    assert!(page["dropped_before"].as_u64().unwrap() > 0);
    assert_eq!(page["text"], "ä");
    assert!(page["next_offset"].as_u64().unwrap() > page["offset"].as_u64().unwrap());
    let long = ctx
        .jobs
        .start(
            &ctx.run_id,
            ctx.sandbox.clone(),
            ctx.workspace.clone(),
            false,
            vec!["/bin/sh".into(), "-c".into(), "sleep 20".into()],
            ".".into(),
        )
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        ctx.jobs
            .wait(&ctx.run_id, &long, 30000, cancel)
            .await
            .unwrap_err()
            .code,
        "cancelled"
    );
    ctx.jobs.status(&ctx.run_id, &long, true).await.unwrap();
    let stopped = ctx
        .jobs
        .wait(&ctx.run_id, &long, 5000, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        stopped["output"]["cancelled"] == true || stopped["error"] == "cancelled",
        "{stopped}"
    );
    ctx.jobs.cleanup(&ctx.run_id).await.unwrap();
}

#[tokio::test]
async fn real_browser_exercises_input_catches_failures_and_returns_screenshot() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path()).await;
    std::fs::write(dir.path().join("game.html"),r#"<!doctype html><body><button id="start">Start</button><span id="score">0</span><canvas id="game"></canvas><script>
    let playing=false;start.onclick=()=>playing=true;document.onkeydown=e=>{if(playing&&e.key==='ArrowLeft')score.textContent=String(+score.textContent+1)};
    </script>"#).unwrap();
    std::fs::write(dir.path().join("style.css"), "body {color:black}").unwrap();
    let args = json!({"path":"game.html","actions":[{"type":"click","selector":"#start"},{"type":"press","key":"ArrowLeft"},{"type":"expect_text","selector":"#score","text":"1"}],"verification":{"criteria":["Start and keyboard movement update score"],"paths":["style.css"]}});
    let good = call(&ctx, "browser.check", args.clone()).await.unwrap();
    assert_eq!(good.data["passed"], true, "{}", good.data);
    assert_eq!(good.data["verification"]["passed"], true);
    assert!(good.data["verification"]["files"]["game.html"].is_string());
    assert_eq!(good.images.len(), 1);
    good.images[0].validate().unwrap();
    assert!(!good.data.to_string().contains("base64"));
    std::fs::write(dir.path().join("game.html"),"<body><button id=start>Start</button><span id=score>0</span><script>throw new Error('broken startup')</script>").unwrap();
    let bad = call(&ctx, "browser.check", args).await.unwrap();
    assert_eq!(bad.images.len(), 1);
    assert_eq!(bad.data["passed"], false);
    assert_eq!(bad.data["verification"]["passed"], false);
    assert!(!bad.data["errors"].as_array().unwrap().is_empty());
    let load_only = call(
        &ctx,
        "browser.check",
        json!({"path":"game.html","actions":[],"screenshot":false}),
    )
    .await
    .unwrap();
    assert_eq!(load_only.data["passed"], false);
}

#[tokio::test]
async fn source_inventory_does_not_emit_a_passing_verification() {
    if std::env::var_os("REQUIRE_SANDBOX").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("index.html"),
        "<script>const STEPS = [ const BROKEN = 1;</script>",
    )
    .unwrap();
    let ctx = context(dir.path()).await;
    let result = call(&ctx, "process.run", json!({
        "argv":["python3","-c","import re;s=open('/workspace/index.html',encoding='utf-8').read();print(repr(s[:220]))"],
        "verification":{"criteria":["Inspect the STEPS source"],"paths":["index.html"]}
    })).await.unwrap();
    assert_eq!(result.data["exit_code"], 0);
    assert!(result.data["stdout"].as_str().unwrap().contains("BROKEN"));
    assert!(
        result.data["verification_rejected"]
            .as_str()
            .unwrap()
            .contains("no verification.kind")
    );
    assert!(result.data.get("verification").is_none(), "{}", result.data);
}

#[tokio::test]
async fn browser_startup_syntax_error_reports_location_before_interaction() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path()).await;
    let args = json!({"path":"index.html","screenshot":false,
        "actions":[{"type":"click","selector":"#next"},{"type":"expect_text","selector":"#next","text":"Model"}],
        "verification":{"criteria":["Next advances to Model"],"paths":["index.html"]}});
    // The missing button is a consequence of startup failure, not a bad selector.
    std::fs::write(
        dir.path().join("index.html"),
        "<!doctype html><body>Loop\n<script>\nconst STEPS = [\nconst broken = 1;\n</script>",
    )
    .unwrap();
    let failed = call(&ctx, "browser.check", args.clone()).await.unwrap();
    assert_eq!(failed.data["passed"], false, "{}", failed.data);
    assert_eq!(failed.data["failure_phase"], "startup");
    assert_eq!(failed.data["actions"], json!([]));
    assert!(failed.data.get("failed_action").is_none());
    let error = &failed.data["page_errors"][0];
    assert_eq!(error["path"], "index.html", "{}", failed.data);
    assert_eq!(error["line"], 4);
    assert_eq!(error["column"], 1);
    assert_eq!(error["source_line"], "const broken = 1;");
    assert!(failed.data["errors"].to_string().contains("index.html:4:1"));
    assert!(!failed.data["errors"].to_string().contains("Timeout"));
    // Check external-script locations, too; HTML line numbers cannot stand in.
    std::fs::write(
        dir.path().join("index.html"),
        "<body>Loop<script src='app.js'></script>",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("app.js"),
        "// Broken\nconst STEPS = [\nconst broken = 1;",
    )
    .unwrap();
    let external = call(&ctx, "browser.check", args.clone()).await.unwrap();
    assert_eq!(
        external.data["page_errors"][0]["path"], "app.js",
        "{}",
        external.data
    );
    assert_eq!(external.data["page_errors"][0]["line"], 3);
    std::fs::write(dir.path().join("app.js"), "document.body.innerHTML='<button id=next>Task</button>';document.getElementById('next').onclick=e=>e.target.textContent='Model';").unwrap();
    let corrected = call(&ctx, "browser.check", args).await.unwrap();
    assert_eq!(corrected.data["passed"], true, "{}", corrected.data);
    assert!(corrected.data.get("page_errors").is_none());
}

#[tokio::test]
async fn browser_failure_identifies_wrong_tab_expectation_and_missing_selector() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path()).await;
    let html = r#"<!doctype html><title>Tabs</title><body>
      <button id="explain" role="tab" aria-selected="true">Explanation</button>
      <button id="example" role="tab" aria-selected="false">Example</button>
      <section id="explanation">Explanation content</section>
      <section id="sample" hidden>Example content</section>
      <button id="next">Next</button><output id="count">0</output>
      <script>
      const el = id => document.getElementById(id);
      function select(example) {
        el('explanation').hidden = example; el('sample').hidden = !example;
        el('explain').setAttribute('aria-selected', String(!example));
        el('example').setAttribute('aria-selected', String(example));
      }
      el('explain').onclick = () => select(false);
      el('example').onclick = () => select(true);
      el('next').onclick = () => el('count').textContent = 'unexpected action';
      </script>"#;
    std::fs::write(dir.path().join("tabs.html"), html).unwrap();
    let failed = call(
        &ctx,
        "browser.check",
        json!({
            "path":"tabs.html",
            "actions":[
                {"type":"click","selector":"#example"},
                {"type":"expect_visible","selector":"#sample"},
                {"type":"expect_visible","selector":"#explanation"},
                {"type":"click","selector":"#next"}
            ],
            "verification":{"criteria":["Switch between tabs"],"paths":["tabs.html"]}
        }),
    )
    .await
    .unwrap();
    assert_eq!(failed.data["passed"], false);
    assert_eq!(failed.data["verification"]["passed"], false);
    assert_eq!(failed.data["actions"].as_array().unwrap().len(), 2);
    let failure = &failed.data["failed_action"];
    assert_eq!(failure["index"], 3);
    assert_eq!(failure["selector"], "#explanation");
    assert_eq!(failure["last_interaction"]["selector"], "#example");
    assert_eq!(failure["last_interaction"]["index"], 1);
    assert_eq!(failure["target"]["count"], 1);
    assert_eq!(failure["target"]["visible"], false);
    assert_eq!(failure["target"]["hidden"], true);
    assert_eq!(failure["target"]["text"], "Explanation content");
    assert!(
        failed.data["controls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "explain" && c["selected"] == "false")
    );
    assert_eq!(failed.data["title"], "Tabs");
    let text = failed.data["text"].as_str().unwrap();
    assert!(text.contains("Example content"));
    assert!(!text.contains("Explanation content"));
    assert!(!text.contains("unexpected action"));
    assert_eq!(failed.images.len(), 1);

    // Correct the check's missing click, preserving the working application.
    let corrected = call(&ctx, "browser.check", json!({
        "path":"tabs.html","screenshot":false,
        "actions":[
            {"type":"click","selector":"#example"},
            {"type":"expect_visible","selector":"#sample"},
            {"type":"click","selector":"#explain"},
            {"type":"expect_visible","selector":"#explanation"}
        ],
        "verification":{"criteria":["Switch between tabs"],"paths":["tabs.html"],"replaces":"invocation_test"}
    })).await.unwrap();
    assert_eq!(corrected.data["passed"], true, "{}", corrected.data);
    assert_eq!(corrected.data["assertions"], 2);
    assert!(corrected.data.get("failed_action").is_none());

    let missing = call(
        &ctx,
        "browser.check",
        json!({
            "path":"tabs.html","screenshot":false,
            "actions":[{"type":"click","selector":"#dot3"}]
        }),
    )
    .await
    .unwrap();
    assert_eq!(missing.data["passed"], false);
    assert_eq!(missing.data["failed_action"]["index"], 1);
    assert_eq!(missing.data["failed_action"]["target"]["count"], 0);
    assert!(
        missing.data["failed_action"]
            .get("last_interaction")
            .is_none()
    );
    assert!(
        missing.data["controls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "explain")
    );

    // Diagnostic observation must not re-evaluate a failed predicate.
    let predicate = "(() => { const el = document.getElementById('count'); el.textContent = String(Number(el.textContent) + 1); throw new Error('predicate failure'); })()";
    let expression = call(
        &ctx,
        "browser.check",
        json!({
            "path":"tabs.html","screenshot":false,
            "actions":[{"type":"expect","expression":predicate}]
        }),
    )
    .await
    .unwrap();
    assert_eq!(expression.data["passed"], false);
    assert_eq!(expression.data["failed_action"]["expression"], predicate);
    assert!(expression.data["text"].as_str().unwrap().ends_with('1'));
    assert!(
        expression.data["errors"]
            .to_string()
            .contains("predicate failure")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("tabs.html")).unwrap(),
        html
    );
}

use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use werk_worker::{context::validate_messages, policy::*, profiles::AgentProfile, providers::*};

struct JobProvider {
    step: AtomicUsize,
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait::async_trait]
impl ModelProvider for JobProvider {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tool_calling: Support::Supported,
            ..Default::default()
        }
    }
    fn identity(&self) -> String {
        "job-fixture".into()
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        validate_messages(&request.messages)?;
        let job = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .filter_map(|m| serde_json::from_str::<Value>(&m.content).ok())
            .find_map(|v| v["data"]["job_id"].as_str().map(str::to_owned));
        self.requests.lock().unwrap().push(request.clone());
        let tc = |name: &str, arguments: Value| ToolCall {
            id: id("call"),
            name: name.into(),
            arguments,
        };
        let turn = match self.step.fetch_add(1, Ordering::SeqCst) {
            0 => ScriptedTurn::calls(
                "",
                vec![
                    tc(
                        "process.start",
                        json!({"argv":["/bin/sh","-c","printf 'ready'; sleep 2; printf 'done'"]}),
                    ),
                    tc("files.read", json!({"path":"ready.txt"})),
                ],
            ),
            1 => ScriptedTurn::calls(
                "",
                vec![
                    tc("process.log", json!({"job_id":job,"length":100})),
                    tc("process.wait", json!({"job_id":job,"timeout_ms":5000})),
                ],
            ),
            2 => ScriptedTurn::calls(
                "",
                vec![
                    tc("files.read", json!({"path":"ready.txt"})),
                    tc("files.search", json!({"path":".","query":"[","regex":true})),
                ],
            ),
            _ => ScriptedTurn::text("Finished"),
        };
        ScriptedProvider::new(vec![turn]).stream(request).await
    }
}
#[tokio::test]
async fn engine_background_wait_preserves_tool_groups_and_releases_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ready.txt"), "ready").unwrap();
    if let Err(error) = Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let provider = Arc::new(JobProvider {
        step: AtomicUsize::new(0),
        requests: Mutex::new(vec![]),
    });
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let names = [
        "process.start",
        "process.wait",
        "process.log",
        "files.read",
        "files.search",
    ];
    let profile = AgentProfile {
        schema_version: 1,
        name: "job-test".into(),
        prompt: "Test background jobs".into(),
        tools: names.map(str::to_owned).to_vec(),
        skills: vec![],
    };
    let policy = HostPolicy {
        workspace_writable: true,
        tool_permissions: names.map(|s| (s.into(), ToolPermission::Allow)).into(),
        ..Default::default()
    };
    let session = engine
        .create_session(profile, dir.path(), policy, Value::Null)
        .await
        .unwrap();
    let run = session
        .submit(
            "Read output after the job",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), run.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    let status = run.status().await.unwrap();
    let outcomes: Vec<_> = status
        .invocations
        .iter()
        .map(|i| i.result.as_ref().unwrap())
        .collect();
    assert_eq!(outcomes[1].error_code.as_deref(), Some("workspace_busy"));
    assert_eq!(outcomes[3].data["status"], "finished");
    assert_eq!(outcomes[4].data["content"], "ready");
    assert_eq!(outcomes[5].error_code.as_deref(), Some("invalid_pattern"));
    assert!(
        outcomes[5].data["message"]
            .as_str()
            .unwrap()
            .contains("literal")
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    let last = requests.last().unwrap();
    assert_eq!(
        last.messages
            .iter()
            .filter(|m| m
                .content
                .contains("\"event\":\"background_jobs_completed\""))
            .count(),
        1
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn browser_evidence_allows_completion_only_for_current_file_hashes() {
    if std::env::var_os("REQUIRE_BROWSER").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tc = |name: &str, arguments: Value| {
        ScriptedTurn::calls(
            "",
            vec![ToolCall {
                id: id("call"),
                name: name.into(),
                arguments,
            }],
        )
    };
    let check = |text: &str| {
        tc(
            "browser.check",
            json!({"path":"game.html","screenshot":false,"actions":[{"type":"expect_text","selector":"#score","text":text}],"verification":{"criteria":["Visible score is correct"],"paths":["game.html"]}}),
        )
    };
    let initial = "<!doctype html><body><span id=score>one</span>";
    let provider = Arc::new(ScriptedProvider::new(vec![
        tc("files.write", json!({"path":"game.html","content":initial})),
        check("one"),
        tc(
            "files.edit",
            json!({"path":"game.html","expected_hash":hash(initial),"old":"one","new":"two"}),
        ),
        ScriptedTurn::text("Stale check must not finish"),
        check("two"),
        ScriptedTurn::text("Current version verified"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let names = ["files.write", "files.edit", "browser.check"];
    let profile = AgentProfile {
        schema_version: 1,
        name: "coding-basic".into(),
        prompt: "Test browser evidence".into(),
        tools: names.map(str::to_owned).to_vec(),
        skills: vec![],
    };
    let policy = HostPolicy {
        workspace_writable: true,
        tool_permissions: names.map(|s| (s.into(), ToolPermission::Allow)).into(),
        ..Default::default()
    };
    let session = engine
        .create_session(profile, dir.path(), policy, Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("Implement a score", "start", Limits::default(), None)
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(result.text.as_deref(), Some("Current version verified"));
    let state = run.status().await.unwrap();
    let checks: Vec<_> = state
        .invocations
        .iter()
        .filter(|i| i.call.name == "browser.check")
        .collect();
    assert_eq!(checks.len(), 2);
    for check in checks {
        assert_eq!(
            check.result.as_ref().unwrap().data["verification"]["passed"],
            true
        );
    }
    for request in provider.requests() {
        validate_messages(&request.messages).unwrap();
    }
    engine.close().await.unwrap();
    // Preparation pins the declared dependencies, not just the entry HTML.
    let ctx = context(dir.path()).await;
    std::fs::write(dir.path().join("game.js"), "let n=1").unwrap();
    let tool = ToolRegistry::builtins()
        .unwrap()
        .get("browser.check")
        .unwrap();
    let prepared=tool.prepare(json!({"path":"game.html","actions":[],"verification":{"criteria":["state"],"paths":["game.html","game.js"]}}),&ctx).await.unwrap();
    std::fs::write(dir.path().join("game.js"), "let n=2").unwrap();
    assert_eq!(
        tool.execute(prepared, &ctx).await.unwrap_err().code,
        "write_conflict"
    );
}

#[tokio::test]
async fn small_search_pages_advance_without_false_endless_continuations() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("long.txt"),
        format!("needle{}\nneedle{}\n", "x".repeat(3000), "y".repeat(3000)),
    )
    .unwrap();
    let turn = |offset| {
        ScriptedTurn::calls(
            "",
            vec![ToolCall {
                id: id("call"),
                name: "files.search".into(),
                arguments: json!({"path":"long.txt","query":"needle","offset":offset,"limit":1}),
            }],
        )
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        turn(0),
        turn(1),
        ScriptedTurn::text("Read both matches"),
    ]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let profile = AgentProfile {
        schema_version: 1,
        name: "search-test".into(),
        prompt: "Search".into(),
        tools: vec!["files.search".into()],
        skills: vec![],
    };
    let policy = HostPolicy {
        tool_permissions: [("files.search".into(), ToolPermission::Allow)].into(),
        ..Default::default()
    };
    let session = engine
        .create_session(profile, dir.path(), policy, Value::Null)
        .await
        .unwrap();
    let run = session
        .submit(
            "Find matches",
            "start",
            Limits {
                max_output_bytes: 512,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let state = run.status().await.unwrap();
    let a = &state.invocations[0].result.as_ref().unwrap().data;
    let b = &state.invocations[1].result.as_ref().unwrap().data;
    assert_eq!(a["next_offset"], 1);
    assert_eq!(a["matches"][0]["line"], 1);
    assert_eq!(b["matches"][0]["line"], 2);
    assert_eq!(b["has_more"], false);
    assert!(b["next_offset"].is_null());
    assert!(serde_json::to_vec(a).unwrap().len() <= 512);
    assert!(serde_json::to_vec(b).unwrap().len() <= 512);
    engine.close().await.unwrap();
}
