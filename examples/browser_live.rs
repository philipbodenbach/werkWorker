//! Opt-in evaluation through the ordinary WerkProvider/engine, without scripted decisions.
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::Parser;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use werk_worker::{browser_policy::*, policy::*, profiles::AgentProfile, providers::*, *};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    secret_env: Option<String>,
    #[arg(long, default_value = "visual-puzzle")]
    case: String,
    #[arg(long, default_value = "vision")]
    perception: String,
    #[arg(long, default_value = "browser-live-result")]
    output: PathBuf,
    #[arg(long, default_value_t = 900)]
    seconds: u64,
    #[arg(long, default_value_t = 60)]
    steps: u32,
    /// Exercise a bundled profile's full tool/context overhead.
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    reserve_output_tokens: Option<usize>,
    /// Replay an exact user task instead of the case's default prompt.
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    /// Additional exact origins explicitly permitted by the host for this run.
    #[arg(long)]
    origin: Vec<String>,
}

const PUZZLE: &str = r#"<!doctype html><style>body{margin:0}</style><canvas width="1024" height="700"></canvas><script>
const c=document.querySelector('canvas'),g=c.getContext('2d');let stage=0;
const layouts=[[[200,220],[670,340],[430,540]],[[740,180],[260,480],[500,320]],[[520,200],[780,480],[220,380]]];
function draw(){g.fillStyle='#fafafa';g.fillRect(0,0,1024,700);if(stage===3){g.fillStyle='#00bb44';g.fillRect(0,0,1024,700);return;}
const p=layouts[stage];g.fillStyle='#1666ee';g.beginPath();g.arc(...p[0],60,0,7);g.fill();g.fillStyle='#22bb55';g.fillRect(p[1][0]-55,p[1][1]-55,110,110);g.fillStyle='#ee3333';g.beginPath();g.moveTo(p[2][0],p[2][1]-65);g.lineTo(p[2][0]-60,p[2][1]+50);g.lineTo(p[2][0]+60,p[2][1]+50);g.fill();}
c.onclick=e=>{if(stage===3)return;const p=layouts[stage][stage];if(Math.hypot(e.offsetX-p[0],e.offsetY-p[1])<50){stage++;draw();}};draw();</script>"#;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config: WerkConfig = serde_json::from_slice(&std::fs::read(&args.config)?)?;
    let perception: Perception = serde_json::from_value(json!(args.perception))?;
    let settings = BrowserSettings {
        perception,
        lifetime_seconds: args.seconds,
        idle_seconds: args.seconds.min(3600),
        max_actions: 400,
    };
    settings.validate()?;
    std::fs::create_dir_all(&args.output)?;
    let workspace = tempfile::tempdir()?;
    std::fs::write(workspace.path().join("puzzle.html"), PUZZLE)?;
    std::fs::write(
        workspace.path().join("index.html"),
        "<a href='form.html'>Open registration</a>",
    )?;
    std::fs::write(
        workspace.path().join("form.html"),
        r#"<label>Name<input id="name"></label><label>City<select id="city"><option>Berlin</option><option>Hamburg</option></select></label><button onclick="document.querySelector('output').textContent='Registered: '+document.querySelector('#name').value+' / '+document.querySelector('#city').value">Register</button><output></output>"#,
    )?;
    let (target, prompt, criterion) = match args.case.as_str() {
        "visual-puzzle" => (
            "http://workspace.invalid",
            "Open local puzzle.html. Using only the visible image, click the blue circle, then the green square, then the red triangle. The shapes move after each correct click. Verify that the whole canvas turns green.",
            "Independent final frame has green pixels at three separated locations",
        ),
        "form" => (
            "http://workspace.invalid",
            "Open local index.html, follow the registration link, register Ada in Hamburg and verify the confirmation.",
            "Observed confirmation Registered: Ada / Hamburg after navigation and input",
        ),
        "jspaint" => (
            "https://jspaint.app",
            "Open https://jspaint.app and draw a house with a red roof, two windows and a tree. Inspect intermediate images and verify the finished drawing.",
            "Human must assess final frame: house, red roof, exactly two windows, tree",
        ),
        "jspaint-house" => (
            "https://jspaint.app",
            "Open https://jspaint.app. Work alone using only the browser UI. Draw a simple house with a black rectangular body and a red filled triangular roof, on a green filled meadow under a blue filled sky. Add white clouds and a yellow filled sun. Choose each appropriate tool and color. Draw the house rectangle by dragging between opposite corners. Draw the roof outline with the pencil in one closed triangular drag path, then fill it red. Keep planning and progress messages brief. Inspect screenshots returned by actions; request extra observations only when needed. Correct visible mistakes and inspect the finished image. Leave the browser open and briefly report the result.",
            "Human must assess final frame: house, filled red roof, green meadow, blue sky, white clouds, yellow sun; tool trace must show rectangle corners and closed pencil triangle",
        ),
        "wordle" => (
            "https://www.nytimes.com",
            "Open https://www.nytimes.com/games/wordle/index.html and play a valid round using visible feedback. Report the observed outcome, including if you do not solve it.",
            "Human must verify submitted guesses and final visible outcome; winning is not guaranteed",
        ),
        "chess" => (
            "https://lichess.org",
            "Open https://lichess.org and start a local practice game against the computer if available. Make at least three legal moves using the visible board and observe replies. Do not create an account or enter multiplayer matchmaking.",
            "Human must verify three legal moves with replies or a concrete compatibility limitation",
        ),
        "pool" => (
            "https://www.crazygames.com",
            "Open https://www.crazygames.com/game/8-ball-pool-billiards-multiplayer and try an available practice mode. Take three aimed shots with visible feedback. Do not register, buy anything or enter multiplayer matchmaking.",
            "Human must verify three shots and observed table changes or a concrete compatibility limitation",
        ),
        _ => {
            return Err(WorkerError::new(
                "eval_case",
                "Choose visual-puzzle, form, jspaint, jspaint-house, wordle, chess or pool",
            ));
        }
    };
    let prompt_override = args
        .prompt_file
        .as_ref()
        .map(std::fs::read_to_string)
        .transpose()?;
    let prompt = prompt_override.as_deref().unwrap_or(prompt);
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .is_some_and(|o| !o.stdout.is_empty());
    let limits = Limits {
        max_steps: args.steps,
        max_tool_invocations: 400,
        max_model_requests: args.steps,
        max_wall_ms: args.seconds * 1000,
        context_tokens: config.capabilities.context_tokens.unwrap_or(32768),
        reserve_output_tokens: args
            .reserve_output_tokens
            .unwrap_or(Limits::default().reserve_output_tokens),
        ..Limits::default()
    };
    let mut report = json!({"commit":commit,"dirty":dirty,"model":config.model,"quantization":"as configured in model ID; otherwise unknown","backend":config.base_url,"perception":perception,"budgets":settings,"run_limits":limits,"case":args.case,"target":target,"criterion":criterion,"status":"connecting","success":false});
    let report_path = args.output.join("report.json");
    let started = std::time::Instant::now();
    let provider = match WerkProvider::connect(
        config,
        args.secret_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok()),
    )
    .await
    {
        Ok(provider) => provider,
        Err(error) => {
            report["status"] = json!("backend_unavailable");
            report["runtime_seconds"] = json!(started.elapsed().as_secs_f64());
            report["tool_count"] = json!(0);
            report["image_count"] = json!(0);
            report["model_requests"] = json!(0);
            report["error_code"] = json!(error.code);
            std::fs::write(report_path, serde_json::to_vec_pretty(&report)?)?;
            return Err(error);
        }
    };
    let engine = WorkerEngine::builder(Arc::new(provider)).build().await?;
    let mut browser = BrowserPolicy {
        settings,
        ..Default::default()
    };
    for site in std::iter::once(target.to_owned()).chain(args.origin) {
        for category in ["browsing", "actions", "unlabelled"] {
            browser.set(&site, category, ToolPermission::Allow)?;
        }
    }
    let profile = if let Some(name) = &args.profile {
        AgentProfile::bundled(name)?
    } else {
        AgentProfile {schema_version:1,name:"browser-live".into(),prompt:"Complete the natural-language browser task, verify visible results and report limitations. Do not use scripts or internal state.".into(),tools:["browser.open","browser.navigate","browser.observe","browser.action","task.plan","skills.list","skills.load"].map(str::to_owned).into(),skills:vec!["browser-use".into()]}
    };
    report["profile"] = json!(profile.name);
    let session = engine
        .create_session(
            profile,
            workspace.path(),
            HostPolicy {
                browser: Some(browser),
                hard_denied_tools: vec!["browser.evaluate".into(), "browser.check".into()],
                tool_permissions: [
                    ("task.plan".into(), ToolPermission::Allow),
                    ("skills.list".into(), ToolPermission::Allow),
                    ("skills.load".into(), ToolPermission::Allow),
                ]
                .into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await?;
    let run = session.submit(prompt, "live-browser", limits, None).await?;
    let mut events = run.events(0);
    let mut trace = Vec::new();
    while let Some(event) = events.next().await {
        let event = event?.event;
        if let EventKind::ToolCompleted {
            invocation_id,
            result,
        } = &event
        {
            let saved = session.snapshot().await?;
            if let Some(invocation) = saved
                .runs
                .iter()
                .flat_map(|r| &r.invocations)
                .find(|i| &i.invocation_id == invocation_id)
            {
                eprintln!(
                    "tool {}: {:?} {:?}",
                    invocation.call.name, result.status, result.error_code
                );
                trace.push(json!({"call":invocation.call,"status":result.status,"error":result.error_code,
                    "data":result.data,"assistant":saved.messages.get(invocation.message_index).map(|m|&m.content)}));
                std::fs::write(
                    args.output.join("trace.json"),
                    serde_json::to_vec_pretty(&trace)?,
                )?;
            }
            if let Some(image) = result.images.last() {
                let bytes = STANDARD
                    .decode(&image.data_base64)
                    .map_err(|_| WorkerError::new("eval_image", "Invalid action frame"))?;
                std::fs::write(args.output.join("latest.jpg"), bytes)?;
            }
        }
        if let EventKind::ApprovalRequested { request } = event {
            // No blanket grant for redirects/CDNs or a newly requested site.
            run.respond_to_approval(
                &request.request_id,
                ApprovalDecision::Deny,
                &id("eval-deny"),
            )
            .await?;
        }
    }
    let result = run.wait().await?;
    std::fs::write(
        args.output.join("session.json"),
        serde_json::to_vec(&session.snapshot().await?)?,
    )?;
    let state = run.status().await?;
    let receipts: Vec<_> = state
        .invocations
        .iter()
        .filter_map(|i| i.result.as_ref())
        .collect();
    report["status"] = json!(result.status);
    report["runtime_seconds"] = json!(started.elapsed().as_secs_f64());
    report["model_requests"] = json!(state.metrics.model_requests);
    report["error_code"] = json!(result.error.as_ref().map(|e| &e.code));
    report["compacted_requests"] = json!(
        state
            .contexts
            .iter()
            .filter(|c| !c.omitted_messages.is_empty())
            .count()
    );
    report["tool_count"] = json!(receipts.len());
    report["image_count"] = json!(receipts.iter().map(|r| r.images.len()).sum::<usize>());
    report["errors"] = json!(
        receipts
            .iter()
            .filter_map(|r| r.error_code.as_ref())
            .collect::<Vec<_>>()
    );
    report["attempt_errors"] = json!(
        state
            .attempts
            .iter()
            .filter_map(|a| a.error.as_ref().map(|e| &e.code))
            .collect::<Vec<_>>()
    );
    report["actions"] = json!(state.invocations.iter().map(|i| json!({"tool":i.call.name,"arguments":i.call.arguments,"error":i.result.as_ref().and_then(|r|r.error_code.as_ref())})).collect::<Vec<_>>());
    report["transport_diagnostics"]=json!(receipts.iter().map(|r|json!({"blocked":r.data["blocked_requests"],"transport":r.data["transport_diagnostics"]})).filter(|v|v["blocked"].as_array().is_some_and(|a|!a.is_empty())||v["transport"].as_array().is_some_and(|a|!a.is_empty())).collect::<Vec<_>>());
    if let Some(image) = receipts.iter().rev().find_map(|r| r.images.last()) {
        let bytes = STANDARD
            .decode(&image.data_base64)
            .map_err(|_| WorkerError::new("eval_image", "Invalid final frame"))?;
        std::fs::write(args.output.join("final.jpg"), &bytes)?;
        if args.case == "visual-puzzle" {
            let frame = image::load_from_memory(&bytes)
                .map_err(|_| WorkerError::new("eval_image", "Invalid final frame"))?
                .to_rgb8();
            let green = [(0.1, 0.1), (0.5, 0.5), (0.9, 0.8)].iter().all(|(x, y)| {
                let p = frame.get_pixel(
                    (x * f64::from(frame.width())) as u32,
                    (y * f64::from(frame.height())) as u32,
                );
                p[1] > 130 && p[0] < 60 && p[2] < 110
            });
            report["success"] = json!(
                green
                    && state
                        .invocations
                        .iter()
                        .filter(|i| i.call.name == "browser.action")
                        .count()
                        >= 3
            );
        }
    }
    if args.case == "form" {
        report["success"] = json!(receipts.iter().any(|r| {
            r.data["text"]
                .as_str()
                .is_some_and(|s| s.contains("Registered: Ada / Hamburg"))
        }));
    }
    if !matches!(args.case.as_str(), "form" | "visual-puzzle") {
        report["success"] = Value::Null;
        report["assessment"] = json!(
            "Manual frame/trajectory review required; engine completion alone is not success"
        );
    }
    std::fs::write(report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("Report: {}", args.output.display());
    engine.close().await
}
