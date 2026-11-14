//! Portable engine lifecycle checks execute the same synthetic native process
//! through real tools and leases on each platform. A missing backend fails.
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use werk_worker::{
    harness::HarnessConfig, policy::*, profiles::AgentProfile, providers::*, runtime::*, *,
};

struct Fixture {
    workspace: Workspace,
    sandbox: Sandbox,
    argv0: String,
    _root: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let sandbox = Sandbox {
            bubblewrap: std::env::var_os("WERK_WORKER_BWRAP")
                .map(Into::into)
                .unwrap_or_else(|| Sandbox::default().bubblewrap),
            native: NativeSandboxConfig {
                macos_path_based_workspace: cfg!(target_os = "macos"),
                macos_system_task_access: cfg!(target_os = "macos"),
                ..Default::default()
            },
            limits: ProcessLimits {
                timeout_ms: 30_000,
                memory_bytes: if cfg!(target_os = "macos") {
                    1 << 40
                } else {
                    1 << 30
                },
                ..Default::default()
            },
        };
        #[cfg(windows)]
        let workspace = sandbox
            .create_native_workspace(root.path().join("workspace"))
            .await
            .unwrap();
        #[cfg(not(windows))]
        let workspace = Workspace::open(root.path()).await.unwrap();
        let name = if cfg!(windows) {
            "lifecycle-probe.exe"
        } else {
            "lifecycle-probe"
        };
        let executable = workspace.path.join(name);
        let build = tempfile::tempdir().unwrap();
        let compiled = build.path().join(name);
        let compilation =
            std::process::Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                .args(["--edition=2024", "-C", "debuginfo=0"])
                .arg(
                    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/native_process_probe.rs"),
                )
                .arg("-o")
                .arg(&compiled)
                .output()
                .unwrap();
        assert!(
            compilation.status.success(),
            "{}",
            String::from_utf8_lossy(&compilation.stderr)
        );
        std::fs::copy(&compiled, &executable).unwrap();
        std::fs::write(workspace.path.join("source.txt"), "observed source").unwrap();
        // Run the completed host-built executable as the positive control,
        // independently of workspace file labels and copy launch behavior.
        // The real sandbox run still executes the byte-identical workspace copy.
        assert_eq!(
            std::fs::read(&compiled).unwrap(),
            std::fs::read(&executable).unwrap()
        );
        std::fs::write(workspace.path.join("control-gate"), "go").unwrap();
        let control = std::process::Command::new(&compiled)
            .args([
                "gated-write",
                "control-ready",
                "control-gate",
                "control-finished",
            ])
            .current_dir(&workspace.path)
            .output()
            .unwrap();
        assert!(
            control.status.success(),
            "{}",
            String::from_utf8_lossy(&control.stderr)
        );
        assert_eq!(
            std::fs::read(workspace.path.join("control-finished")).unwrap(),
            b"finished"
        );
        let argv0 = if cfg!(target_os = "linux") {
            format!("/workspace/{name}")
        } else {
            executable.to_string_lossy().into_owned()
        };
        Self {
            workspace,
            sandbox,
            argv0,
            _root: root,
        }
    }

    fn argv(&self) -> Vec<String> {
        [
            self.argv0.as_str(),
            "gated-write",
            "job-ready",
            "job-gate",
            "job-finished",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    fn cleanup(&self) {
        #[cfg(windows)]
        self.sandbox
            .remove_native_workspace_profiles(&self.workspace)
            .unwrap();
    }
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id("call"),
        name: name.into(),
        arguments,
    }
}

fn profile() -> AgentProfile {
    AgentProfile {
        schema_version: 1,
        name: "native-lifecycle".into(),
        prompt: "Exercise native background lifecycle".into(),
        tools: [
            "process.run",
            "process.start",
            "process.status",
            "process.log",
            "process.wait",
            "process.cancel",
            "files.read",
        ]
        .map(str::to_owned)
        .to_vec(),
        skills: vec![],
    }
}

fn policy(writable: bool) -> HostPolicy {
    HostPolicy {
        workspace_writable: writable,
        tool_permissions: profile()
            .tools
            .into_iter()
            .map(|name| (name, ToolPermission::Allow))
            .collect(),
        ..Default::default()
    }
}

async fn ready(path: &Path) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !path.join("job-ready").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Native process must execute and signal readiness");
}

#[derive(Clone, Copy)]
enum Finish {
    Wait,
    Cancel,
    Hold,
}

const MAX_LIVE_LOG_POLLS: usize = 32;

struct LifecycleProvider {
    step: AtomicUsize,
    live_log_polls: AtomicUsize,
    argv: Vec<String>,
    root: PathBuf,
    finish: Finish,
    held: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl ModelProvider for LifecycleProvider {
    fn identity(&self) -> String {
        "native-lifecycle-provider".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tool_calling: Support::Supported,
            ..Default::default()
        }
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let job = request
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .filter_map(|message| serde_json::from_str::<Value>(&message.content).ok())
            .find_map(|result| result["data"]["job_id"].as_str().map(str::to_owned));
        let turn = match self.step.fetch_add(1, Ordering::SeqCst) {
            0 => ScriptedTurn::calls(
                "Start a real native job",
                vec![call("process.start", json!({"argv":self.argv}))],
            ),
            1 => {
                ready(&self.root).await;
                if matches!(self.finish, Finish::Hold) {
                    self.held.notify_one();
                    return Ok(Box::pin(futures_util::stream::pending()));
                }
                ScriptedTurn::calls(
                    "Inspect the live job and its held lease",
                    vec![
                        call("process.status", json!({"job_id":job})),
                        // Force an incomplete first observation even when the
                        // pipe drains immediately, exercising the retry path.
                        call("process.log", json!({"job_id":job,"length":1})),
                        call("files.read", json!({"path":"source.txt"})),
                    ],
                )
            }
            2 => {
                // The ready file follows the payload's stdout flush, but the
                // engine's asynchronous pipe reader may not have published it
                // yet. Observe the live log before opening the gate or killing
                // the process, so buffered output after exit cannot pass.
                let live_ready = request
                    .messages
                    .iter()
                    .filter(|message| message.role == Role::Tool)
                    .filter_map(|message| serde_json::from_str::<Value>(&message.content).ok())
                    .any(|result| {
                        result["data"]["text"]
                            .as_str()
                            .is_some_and(|text| text.contains("native job ready"))
                    });
                if !live_ready {
                    assert!(
                        self.live_log_polls.fetch_add(1, Ordering::SeqCst) < MAX_LIVE_LOG_POLLS,
                        "Native output was not published while the process was still running"
                    );
                    self.step.fetch_sub(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    return ScriptedProvider::new(vec![ScriptedTurn::calls(
                        "Wait for the live output reader",
                        vec![call("process.log", json!({"job_id":job,"length":100}))],
                    )])
                    .stream(request)
                    .await;
                }
                let mut calls = Vec::new();
                match self.finish {
                    Finish::Wait => std::fs::write(self.root.join("job-gate"), "go").unwrap(),
                    Finish::Cancel => calls.push(call("process.cancel", json!({"job_id":job}))),
                    Finish::Hold => unreachable!(),
                }
                calls.push(call(
                    "process.wait",
                    json!({"job_id":job,"timeout_ms":10000}),
                ));
                ScriptedTurn::calls("Wait for confirmed cleanup", calls)
            }
            3 => ScriptedTurn::calls(
                "Read output and acquire the released workspace",
                vec![
                    call("process.log", json!({"job_id":job,"length":100})),
                    call("files.read", json!({"path":"source.txt"})),
                ],
            ),
            _ => ScriptedTurn::text("Native lifecycle checked."),
        };
        ScriptedProvider::new(vec![turn]).stream(request).await
    }
}

fn provider(fixture: &Fixture, finish: Finish) -> Arc<LifecycleProvider> {
    Arc::new(LifecycleProvider {
        step: AtomicUsize::new(0),
        live_log_polls: AtomicUsize::new(0),
        argv: fixture.argv(),
        root: fixture.workspace.path.clone(),
        finish,
        held: tokio::sync::Notify::new(),
    })
}

async fn assert_stopped(fixture: &Fixture) {
    // A still-running probe would write within 10 ms after this host-side gate.
    std::fs::write(fixture.workspace.path.join("job-gate"), "go").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !fixture.workspace.path.join("job-finished").exists(),
        "Process wrote after cleanup was promised"
    );
}

async fn background(finish: Finish) {
    let fixture = Fixture::new().await;
    let engine = WorkerEngine::builder(provider(&fixture, finish))
        .sandbox(fixture.sandbox.clone())
        .harness(HarnessConfig::minimal())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(),
            &fixture.workspace.path,
            policy(true),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Exercise background lifecycle",
            "start",
            Limits {
                // This fixture deliberately waits for asynchronous live output
                // through process.log while holding the process's gate closed.
                max_repeated_calls: MAX_LIVE_LOG_POLLS + 2,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(25), run.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    let state = run.status().await.unwrap();
    let by_name = |name: &str| {
        state
            .invocations
            .iter()
            .filter(|i| i.call.name == name)
            .map(|i| i.result.as_ref().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(by_name("process.status")[0].data["status"], "running");
    let logs = by_name("process.log");
    let (finished_log, live_logs) = logs.split_last().unwrap();
    assert!(live_logs.iter().any(|log| {
        log.data["text"]
            .as_str()
            .unwrap()
            .contains("native job ready")
    }));
    let reads = by_name("files.read");
    assert_eq!(reads[0].error_code.as_deref(), Some("workspace_busy"));
    assert_eq!(reads[1].data["content"], "observed source");
    let waited = by_name("process.wait")[0];
    assert_eq!(waited.data["status"], "finished", "{:?}", waited.data);
    match finish {
        Finish::Wait => {
            assert_eq!(waited.data["output"]["exit_code"], 0);
            assert!(
                finished_log.data["text"]
                    .as_str()
                    .unwrap()
                    .contains("native job finished")
            );
            assert_eq!(
                std::fs::read(fixture.workspace.path.join("job-finished")).unwrap(),
                b"finished"
            );
        }
        Finish::Cancel => {
            assert_eq!(waited.data["output"]["cancelled"], true);
            assert_stopped(&fixture).await;
        }
        Finish::Hold => unreachable!(),
    }
    engine.close().await.unwrap();
    fixture.cleanup();
}

#[tokio::test]
async fn background_wait_keeps_live_output_and_workspace_lease_until_completion() {
    background(Finish::Wait).await;
}

#[tokio::test]
async fn background_cancel_waits_for_cleanup_before_releasing_workspace() {
    background(Finish::Cancel).await;
}

#[tokio::test]
async fn engine_close_and_run_abort_stop_owned_background_jobs() {
    for close_engine in [true, false] {
        let fixture = Fixture::new().await;
        let provider = provider(&fixture, Finish::Hold);
        let engine = WorkerEngine::builder(provider.clone())
            .sandbox(fixture.sandbox.clone())
            .harness(HarnessConfig::minimal())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                profile(),
                &fixture.workspace.path,
                policy(true),
                Value::Null,
            )
            .await
            .unwrap();
        let run = session
            .submit("Hold a background job", "start", Limits::default(), None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), provider.held.notified())
            .await
            .unwrap();
        if close_engine {
            tokio::time::timeout(Duration::from_secs(10), engine.close())
                .await
                .unwrap()
                .unwrap();
        } else {
            run.cancel("host abort", "abort").await.unwrap();
            let result = tokio::time::timeout(Duration::from_secs(10), run.wait())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.status, RunStatus::Cancelled);
            engine.close().await.unwrap();
        }
        assert_stopped(&fixture).await;
        fixture.cleanup();
    }
}

#[tokio::test]
async fn host_read_only_policy_is_enforced_inside_native_process() {
    let fixture = Fixture::new().await;
    let scripted = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::calls(
            "Try a process write",
            vec![call(
                "process.run",
                json!({
                    "argv":[fixture.argv0,"write","forbidden.txt","changed"]
                }),
            )],
        ),
        ScriptedTurn::text("Read-only policy checked."),
    ]));
    let engine = WorkerEngine::builder(scripted)
        .sandbox(fixture.sandbox.clone())
        .harness(HarnessConfig::minimal())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(),
            &fixture.workspace.path,
            policy(false),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Check read-only workspace",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), run.wait())
        .await
        .unwrap()
        .unwrap();
    let state = run.status().await.unwrap();
    let result = state.invocations[0].result.as_ref().unwrap();
    assert_eq!(result.status, Outcome::Succeeded, "{:?}", result.data);
    assert_eq!(
        result.data["exit_code"], 77,
        "The real process must launch before its write is denied"
    );
    assert!(!fixture.workspace.path.join("forbidden.txt").exists());
    engine.close().await.unwrap();
    fixture.cleanup();
}
