//! Native qualification for the explicitly provisioned Windows process-tree mode.
//!
//! Set WW_WINDOWS_PROCESS_TREE_PROVISION to the protected manifest and
//! REQUIRE_WINDOWS_PROCESS_TREE=1 to require execution. Without provisioning the
//! acceptance case reports NOT RUN; the missing/malformed configuration tests
//! still run. Existing single-process CI remains a separate gate.
#![cfg(windows)]

use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use werk_worker::{
    harness::HarnessConfig, policy::*, profiles::AgentProfile, providers::*, runtime::*, tools::*,
    *,
};
use windows_sys::Win32::{
    Foundation::*,
    System::{JobObjects::IsProcessInJob, Registry::*, Threading::*},
};

#[path = "fixtures/windows_tree_dns.rs"]
mod tree_dns;

const PROVISION_ENV: &str = "WW_WINDOWS_PROCESS_TREE_PROVISION";
const REQUIRED_ENV: &str = "REQUIRE_WINDOWS_PROCESS_TREE";
const PAYLOAD: &str = "process_tree_payload";

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn registry_open(root: HKEY, path: &str, access: REG_SAM_FLAGS) -> bool {
    let mut key = std::ptr::null_mut();
    let status = unsafe { RegOpenKeyExW(root, wide(path).as_ptr(), 0, access, &mut key) };
    if status == ERROR_SUCCESS {
        unsafe {
            RegCloseKey(key);
        }
    }
    status == ERROR_SUCCESS
}
struct RegistryFixture(String);
impl RegistryFixture {
    fn new() -> Self {
        let path = format!("Software\\WerkWorkerTreeTest-{}", uuid::Uuid::new_v4());
        let mut key = std::ptr::null_mut();
        assert_eq!(
            unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    wide(&path).as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_READ | KEY_WRITE,
                    std::ptr::null(),
                    &mut key,
                    std::ptr::null_mut(),
                )
            },
            ERROR_SUCCESS
        );
        unsafe {
            RegCloseKey(key);
        }
        Self(path)
    }
}
impl Drop for RegistryFixture {
    fn drop(&mut self) {
        let status = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, wide(&self.0).as_ptr()) };
        if status != ERROR_SUCCESS {
            if std::thread::panicking() {
                eprintln!("Synthetic registry key cleanup failed: {status}");
            } else {
                panic!("Synthetic registry key cleanup failed: {status}");
            }
        }
    }
}

fn payload_args() -> [&'static str; 4] {
    ["--ignored", "--exact", PAYLOAD, "--nocapture"]
}
fn write_json(path: impl AsRef<Path>, value: Value) {
    let path = path.as_ref();
    let temporary = path.with_extension("pending");
    fs::write(&temporary, value.to_string()).unwrap();
    fs::rename(temporary, path).unwrap();
}
fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn birth(handle: HANDLE) -> u64 {
    let mut created: FILETIME = unsafe { std::mem::zeroed() };
    let mut exited = created;
    let mut kernel = created;
    let mut user = created;
    assert_ne!(
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) },
        0
    );
    ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64
}

struct Pinned(Vec<OwnedHandle>);
impl Pinned {
    fn from_ids(ids: &[u32]) -> Self {
        let mut result = Self(Vec::new());
        for pid in ids {
            let handle = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                    0,
                    *pid,
                )
            };
            assert!(
                !handle.is_null(),
                "Cannot pin live fixture process {pid}: {}",
                std::io::Error::last_os_error()
            );
            result
                .0
                .push(unsafe { OwnedHandle::from_raw_handle(handle) });
            assert_eq!(
                unsafe { WaitForSingleObject(handle, 0) },
                WAIT_TIMEOUT,
                "Fixture exited before pinning"
            );
        }
        result
    }
    fn assert_receipts(&self, ids: &[u32], directory: &Path) {
        for generation in 0..3 {
            let receipt = read_json(directory.join(format!("receipt-{generation}.json")));
            let pid = receipt["pid"].as_u64().unwrap() as u32;
            let index = ids
                .iter()
                .position(|value| *value == pid)
                .expect("Payload must be in the exact owned Job");
            assert_eq!(
                birth(self.0[index].as_raw_handle()),
                receipt["created"].as_u64().unwrap(),
                "PID identity changed before observation"
            );
            assert_eq!(receipt["generation"], generation);
            assert_eq!(receipt["in_job"], true);
            assert_eq!(receipt["breakaway_denied"], true);
        }
    }
    fn assert_all_stopped(&self) {
        for handle in &self.0 {
            assert_eq!(
                unsafe { WaitForSingleObject(handle.as_raw_handle(), 10_000) },
                WAIT_OBJECT_0,
                "Cleanup returned while a pinned Job member was still alive"
            );
        }
    }
}
impl Drop for Pinned {
    fn drop(&mut self) {
        // Emergency cleanup uses retained handles, never a reopened PID. It
        // cannot convert a failed assertion into successful qualification.
        for handle in &self.0 {
            if unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } == WAIT_TIMEOUT {
                unsafe {
                    TerminateProcess(handle.as_raw_handle(), 77);
                    WaitForSingleObject(handle.as_raw_handle(), 10_000);
                }
            }
        }
    }
}
struct Host(Child);
impl Drop for Host {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct Fixture {
    manifest: PathBuf,
    provision: Value,
    workspace: Workspace,
    sandbox: Sandbox,
}
impl Fixture {
    async fn configured() -> Option<Self> {
        let Some(manifest) = std::env::var_os(PROVISION_ENV).map(PathBuf::from) else {
            assert_ne!(
                std::env::var(REQUIRED_ENV).as_deref(),
                Ok("1"),
                "{REQUIRED_ENV}=1 requires {PROVISION_ENV}; native acceptance did not execute"
            );
            eprintln!(
                "NOT RUN: native process-tree qualification requires {PROVISION_ENV}; set {REQUIRED_ENV}=1 to make missing provisioning fail"
            );
            return None;
        };
        let provision = read_json(&manifest);
        let workspace = Workspace::open(
            provision["workspace"]
                .as_str()
                .expect("Provisioned workspace missing"),
        )
        .await
        .unwrap();
        let sandbox = Sandbox {
            native: NativeSandboxConfig {
                windows_process_tree: Some(WindowsProcessTreeConfig {
                    provisioning: manifest.clone(),
                }),
                ..Default::default()
            },
            limits: ProcessLimits {
                processes: 4,
                timeout_ms: 20_000,
                memory_bytes: 512 << 20,
                cpu_seconds: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        // Verify the chosen host profile before any payload or fixture is run.
        sandbox
            .execution_description(&workspace, true)
            .await
            .unwrap();
        Some(Self {
            manifest,
            provision,
            workspace,
            sandbox,
        })
    }
    fn case(&self, mode: &str) -> tempfile::TempDir {
        let directory = tempfile::Builder::new()
            .prefix("ww-tree-test-")
            .tempdir_in(&self.workspace.path)
            .unwrap();
        // Copy bytes into the fresh provisioned ACL hierarchy; do not copy the
        // host executable's security descriptor into the dedicated workspace.
        fs::write(
            directory.path().join("probe.exe"),
            fs::read(std::env::current_exe().unwrap()).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.path().join("source.txt"),
            b"workspace lease control",
        )
        .unwrap();
        write_json(directory.path().join("plan.json"), json!({"mode":mode}));
        directory
    }
    fn arguments(&self, directory: &Path) -> Value {
        let mut argv = vec![directory.join("probe.exe").to_string_lossy().into_owned()];
        argv.extend(payload_args().map(str::to_owned));
        json!({"argv":argv,"cwd":directory.strip_prefix(&self.workspace.path).unwrap().to_string_lossy()})
    }
    fn snapshot(&self, directory: &Path, expected: usize) -> Pinned {
        let snapshot = self
            .sandbox
            .windows_process_tree_snapshot(&self.workspace)
            .unwrap();
        assert_eq!(snapshot["membership_verified"], true, "{snapshot}");
        assert_eq!(
            snapshot["limits"]["processes"], self.sandbox.limits.processes,
            "{snapshot}"
        );
        assert_eq!(
            snapshot["limits"]["memory_bytes"], self.sandbox.limits.memory_bytes,
            "{snapshot}"
        );
        assert_eq!(
            snapshot["limits"]["cpu_seconds"], self.sandbox.limits.cpu_seconds,
            "{snapshot}"
        );
        assert_eq!(snapshot["limits"]["kill_on_close"], true, "{snapshot}");
        assert_eq!(snapshot["limits"]["breakaway"], false, "{snapshot}");
        let ids: Vec<u32> = serde_json::from_value(snapshot["process_ids"].clone()).unwrap();
        assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), expected);
        assert_eq!(
            ids.len(),
            expected,
            "Trusted runner plus all payload generations must be counted: {snapshot}"
        );
        assert_eq!(snapshot["active_processes"], expected, "{snapshot}");
        let pinned = Pinned::from_ids(&ids);
        if expected == 4 {
            pinned.assert_receipts(&ids, directory);
        }
        println!("NATIVE_TREE_JOB_SNAPSHOT {snapshot}");
        write_json(directory.join("host-snapshot.json"), snapshot);
        pinned
    }
}
async fn wait_for(path: impl AsRef<Path>) {
    let path = path.as_ref();
    tokio::time::timeout(Duration::from_secs(15), async {
        while !path.is_file() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Native fixture did not signal {}", path.display()));
}
fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id("tree-call"),
        name: name.into(),
        arguments,
    }
}
fn profile() -> AgentProfile {
    AgentProfile {
        schema_version: 1,
        name: "windows-tree-qualification".into(),
        prompt: "Exercise native process-tree contracts".into(),
        tools: [
            "process.run",
            "process.start",
            "process.status",
            "process.wait",
            "process.cancel",
            "files.read",
        ]
        .map(str::to_owned)
        .to_vec(),
        skills: vec![],
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Run,
    Cancel,
    Hold,
}
struct Provider {
    step: AtomicUsize,
    arguments: Value,
    source: String,
    mode: Mode,
    observed: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl ModelProvider for Provider {
    fn identity(&self) -> String {
        "native-process-tree-qualification".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tool_calling: Support::Supported,
            ..Default::default()
        }
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let results: Vec<Value> = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .filter_map(|m| serde_json::from_str(&m.content).ok())
            .collect();
        let job = results
            .iter()
            .find_map(|v| v["data"]["job_id"].as_str())
            .map(str::to_owned);
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        let turn = if self.mode == Mode::Run {
            match step {
                0 => ScriptedTurn::calls(
                    "Run a native process tree",
                    vec![call("process.run", self.arguments.clone())],
                ),
                1 => ScriptedTurn::calls(
                    "Acquire workspace after cleanup",
                    vec![call("files.read", json!({"path":self.source}))],
                ),
                _ => ScriptedTurn::text("Native process completed."),
            }
        } else {
            match step {
                0 => ScriptedTurn::calls(
                    "Start a native process tree",
                    vec![call("process.start", self.arguments.clone())],
                ),
                1 => ScriptedTurn::calls(
                    "Check active job and held lease",
                    vec![
                        call("process.status", json!({"job_id":job})),
                        call("files.read", json!({"path":self.source})),
                    ],
                ),
                2 => {
                    assert!(
                        results.iter().any(|v| v["error_code"] == "workspace_busy"),
                        "Background job did not retain its workspace lease: {results:?}"
                    );
                    self.observed.notify_one();
                    if self.mode == Mode::Hold {
                        return Ok(Box::pin(futures_util::stream::pending()));
                    }
                    self.release.notified().await;
                    ScriptedTurn::calls(
                        "Cancel the owned native tree",
                        vec![call("process.cancel", json!({"job_id":job}))],
                    )
                }
                3 => ScriptedTurn::calls(
                    "Wait for confirmed cleanup",
                    vec![call(
                        "process.wait",
                        json!({"job_id":job,"timeout_ms":10000}),
                    )],
                ),
                4 => ScriptedTurn::calls(
                    "Acquire workspace after confirmed cleanup",
                    vec![call("files.read", json!({"path":self.source}))],
                ),
                _ => ScriptedTurn::text("Native background cleanup completed."),
            }
        };
        ScriptedProvider::new(vec![turn]).stream(request).await
    }
}
async fn launch(
    fixture: &Fixture,
    directory: &Path,
    arguments: Value,
    mode: Mode,
) -> (WorkerEngine, RunHandle, Arc<Provider>) {
    let provider = Arc::new(Provider {
        step: AtomicUsize::new(0),
        arguments,
        source: directory
            .join("source.txt")
            .strip_prefix(&fixture.workspace.path)
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        mode,
        observed: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
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
            HostPolicy {
                workspace_writable: true,
                tool_permissions: profile()
                    .tools
                    .into_iter()
                    .map(|name| (name, ToolPermission::Allow))
                    .collect(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Run the native acceptance fixture",
            "qualify",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    (engine, run, provider)
}
async fn finish(run: &RunHandle) -> Value {
    let terminal = tokio::time::timeout(Duration::from_secs(150), run.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(terminal.status, RunStatus::Completed, "{terminal:?}");
    let state = run.status().await.unwrap();
    let reads: Vec<_> = state
        .invocations
        .iter()
        .filter(|i| i.call.name == "files.read")
        .map(|i| i.result.as_ref().unwrap())
        .collect();
    assert_eq!(
        reads.last().unwrap().status,
        Outcome::Succeeded,
        "Lease not released after confirmed cleanup"
    );
    let process = state
        .invocations
        .iter()
        .find(|i| i.call.name == "process.run" || i.call.name == "process.wait")
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert_eq!(process.status, Outcome::Succeeded, "{process:?}");
    if process.data.get("output").is_some() {
        process.data["output"].clone()
    } else {
        process.data.clone()
    }
}

struct LpacProfiles<'a>(&'a Sandbox, &'a Workspace);
impl Drop for LpacProfiles<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.0.remove_native_workspace_profiles(self.1) {
            if std::thread::panicking() {
                eprintln!("LPAC control cleanup failed: {error}");
            } else {
                panic!("LPAC control cleanup failed: {error}");
            }
        }
    }
}

#[tokio::test]
async fn missing_or_malformed_provision_cannot_fall_back_to_single_process() {
    assert!(
        NativeSandboxConfig::default()
            .windows_process_tree
            .is_none()
    );
    let root = tempfile::tempdir().unwrap();
    // A real, working LPAC workspace makes this a fallback regression: failure
    // cannot be attributed to an unprovisioned default-backend workspace.
    let baseline = Sandbox::default();
    let workspace = baseline
        .create_native_workspace(root.path().join("workspace"))
        .await
        .unwrap();
    let _profiles = LpacProfiles(&baseline, &workspace);
    let command = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32/cmd.exe")
        .to_string_lossy()
        .into_owned();
    let argv = vec![command, "/d".into(), "/c".into(), "exit".into(), "0".into()];
    let control = baseline
        .run(&workspace, true, &argv, ".", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        control.exit_code,
        Some(0),
        "The supported LPAC fallback control must really execute"
    );
    for name in ["missing.json", "malformed.json"] {
        let manifest = root.path().join(name);
        if name == "malformed.json" {
            fs::write(&manifest, b"{}").unwrap();
        }
        let sandbox = Sandbox {
            native: NativeSandboxConfig {
                windows_process_tree: Some(WindowsProcessTreeConfig {
                    provisioning: manifest,
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let context = ToolContext {
            workspace_lease: None,
            session_id: "tree-negative".into(),
            run_id: "tree-negative".into(),
            invocation_id: "tree-negative".into(),
            workspace: workspace.clone(),
            writable: true,
            cancel: CancellationToken::new(),
            sandbox,
            jobs: Arc::new(JobManager::default()),
            artifacts: None,
            artifact_refs: vec![],
            skills: Default::default(),
            native_jobs: vec![],
        };
        for tool in ["process.run", "process.start"] {
            let result = ToolRegistry::builtins()
                .unwrap()
                .get(tool)
                .unwrap()
                .prepare(json!({"argv":["must-not-execute.exe"]}), &context)
                .await;
            assert!(result.is_err(), "{tool} admitted {name}");
        }
        assert!(
            context
                .sandbox
                .run(&workspace, true, &argv, ".", CancellationToken::new())
                .await
                .is_err()
        );
    }
    baseline
        .remove_native_workspace_profiles(&workspace)
        .unwrap();
}

#[tokio::test]
async fn required_native_process_tree_acceptance() {
    let Some(mut fixture) = Fixture::configured().await else {
        return;
    };
    // The fixed manifest workspace is reused serially. Each case gets a fresh
    // child directory and completes native cleanup before the next starts.
    for scenario in ["normal", "timeout", "cancel", "abort", "engine-close"] {
        let directory = fixture.case("tree");
        fixture.sandbox.limits.timeout_ms = if scenario == "timeout" { 5_000 } else { 20_000 };
        let mode = match scenario {
            "cancel" => Mode::Cancel,
            "abort" | "engine-close" => Mode::Hold,
            _ => Mode::Run,
        };
        let mut arguments = fixture.arguments(directory.path());
        if mode == Mode::Run {
            let input = "tree-stdin\nGrüße 世界\n";
            write_json(
                directory.path().join("plan.json"),
                json!({"mode":"tree","stdin":input}),
            );
            arguments["stdin"] = json!(input);
        }
        let (engine, run, provider) = launch(&fixture, directory.path(), arguments, mode).await;
        for generation in 0..3 {
            wait_for(directory.path().join(format!("receipt-{generation}.json"))).await;
        }
        wait_for(directory.path().join("budget.json")).await;
        let pinned = fixture.snapshot(directory.path(), 4);
        assert_eq!(
            read_json(directory.path().join("budget.json"))["extra_child_denied"],
            true,
            "Finite Job process budget must include helpers and the trusted runner"
        );
        if mode != Mode::Run {
            tokio::time::timeout(Duration::from_secs(15), provider.observed.notified())
                .await
                .unwrap();
        }
        match scenario {
            "normal" => fs::write(directory.path().join("exit-gate"), b"finish").unwrap(),
            "cancel" => provider.release.notify_one(),
            "abort" => {
                run.cancel("qualification abort", "abort").await.unwrap();
            }
            "engine-close" => {
                engine.close().await.unwrap();
            }
            _ => {}
        }
        if matches!(scenario, "abort" | "engine-close") {
            tokio::time::timeout(Duration::from_secs(15), run.wait())
                .await
                .unwrap()
                .unwrap();
        } else {
            let output = finish(&run).await;
            match scenario {
                "normal" => {
                    assert_eq!(output["exit_code"], 0);
                    assert!(
                        output["stdout"]
                            .as_str()
                            .unwrap()
                            .contains("TREE_GENERATION_2")
                    );
                }
                "timeout" => assert_eq!(output["timed_out"], true),
                "cancel" => assert_eq!(output["cancelled"], true),
                _ => unreachable!(),
            }
        }
        // The OS handles are checked after the tool/engine cleanup promise.
        pinned.assert_all_stopped();
        assert!(
            fixture
                .sandbox
                .windows_process_tree_snapshot(&fixture.workspace)
                .is_err(),
            "Completed invocation remains registered"
        );
        engine.close().await.unwrap();
        println!("NATIVE_TREE_LIFECYCLE_OK {scenario}");
    }
    fixture.sandbox.limits.timeout_ms = 20_000;
    dns_broker_checks(&fixture).await;
    boundary_checks(&fixture).await;
    inbound_loopback_checks(&fixture).await;
    resource_checks(&mut fixture).await;
    offline_cargo(&mut fixture).await;
    fixture.sandbox.limits = ProcessLimits {
        processes: 4,
        timeout_ms: 20_000,
        memory_bytes: 512 << 20,
        cpu_seconds: 10,
        ..Default::default()
    };
    abrupt_host_death(&fixture).await;
}

async fn abrupt_host_death(fixture: &Fixture) {
    let directory = fixture.case("tree");
    let mut host = Host(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "process_tree_host_payload",
                "--nocapture",
            ])
            .env(PROVISION_ENV, &fixture.manifest)
            .env(REQUIRED_ENV, "1")
            .env("WW_TREE_CASE_DIRECTORY", directory.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for(directory.path().join("host-snapshot.json")).await;
    let snapshot = read_json(directory.path().join("host-snapshot.json"));
    let ids: Vec<u32> = serde_json::from_value(snapshot["process_ids"].clone()).unwrap();
    assert_eq!(ids.len(), 4);
    let pinned = Pinned::from_ids(&ids);
    pinned.assert_receipts(&ids, directory.path());
    host.0.kill().unwrap();
    host.0.wait().unwrap();
    pinned.assert_all_stopped();
    let marker = fixture
        .manifest
        .parent()
        .unwrap()
        .join("incomplete-run.json");
    assert!(
        marker.is_file(),
        "Abrupt host exit must retain the protected incomplete-run receipt"
    );
    let next = fixture.case("restart-must-not-run");
    let context = ToolContext {
        workspace_lease: None,
        session_id: "after-crash".into(),
        run_id: "after-crash".into(),
        invocation_id: "after-crash".into(),
        workspace: fixture.workspace.clone(),
        writable: true,
        cancel: CancellationToken::new(),
        sandbox: fixture.sandbox.clone(),
        jobs: Arc::new(JobManager::default()),
        artifacts: None,
        artifact_refs: vec![],
        skills: Default::default(),
        native_jobs: vec![],
    };
    let tool = ToolRegistry::builtins()
        .unwrap()
        .get("process.run")
        .unwrap();
    let prepared = tool
        .prepare(fixture.arguments(next.path()), &context)
        .await
        .unwrap();
    let error = tool.execute(prepared, &context).await.expect_err("A subsequent host invocation must refuse the incomplete-run receipt even after this observer sees stopped processes");
    assert_eq!(error.code, "cleanup_required", "{error}");
    assert!(!next.path().join("restart-executed").exists());
    assert!(
        marker.is_file(),
        "A failed restart must not discard the cleanup receipt"
    );
    // Deliberately retained for explicit trusted recovery. This is the final
    // acceptance case, so no later invocation can hide an automatic recovery.
    println!(
        "NATIVE_TREE_HOST_DEATH_AND_RESTART_DENIAL_OK marker={}",
        marker.display()
    );
}

#[tokio::test]
#[ignore = "trusted subprocess fixture for abrupt host-death qualification"]
async fn process_tree_host_payload() {
    let fixture = Fixture::configured().await.unwrap();
    let directory = PathBuf::from(std::env::var_os("WW_TREE_CASE_DIRECTORY").unwrap());
    let (_engine, _run, provider) = launch(
        &fixture,
        &directory,
        fixture.arguments(&directory),
        Mode::Hold,
    )
    .await;
    for generation in 0..3 {
        wait_for(directory.join(format!("receipt-{generation}.json"))).await;
    }
    wait_for(directory.join("budget.json")).await;
    tokio::time::timeout(Duration::from_secs(15), provider.observed.notified())
        .await
        .unwrap();
    let _pinned = fixture.snapshot(&directory, 4);
    // Only process observation handles are retained. Holding an extra Job
    // handle here or in the external observer would invalidate kill-on-close.
    std::future::pending::<()>().await;
}

#[test]
fn native_dns_control_uses_only_explicit_loopback_server() {
    let server = tree_dns::Server::new();
    let name = format!("host-{}.ww-tree.test.", uuid::Uuid::new_v4().simple());
    server.positive_control(&name);
    println!("NATIVE_DNS_SYNTHETIC_HOST_CONTROL_OK");
}

async fn dns_broker_checks(fixture: &Fixture) {
    let server = tree_dns::Server::new();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    server.positive_control(&format!("before-{nonce}.ww-tree.test."));
    let directory = fixture.case("dns");
    write_json(
        directory.path().join("plan.json"),
        json!({"mode":"dns","dns_server":server.address(),"dns_nonce":nonce}),
    );
    let (engine, run, _) = launch(
        fixture,
        directory.path(),
        fixture.arguments(directory.path()),
        Mode::Run,
    )
    .await;
    for generation in 0..3 {
        wait_for(directory.path().join(format!("receipt-{generation}.json"))).await;
    }
    wait_for(directory.path().join("budget.json")).await;
    let pinned = fixture.snapshot(directory.path(), 4);
    fs::write(directory.path().join("dns-gate"), b"query").unwrap();
    for generation in 0..3 {
        wait_for(
            directory
                .path()
                .join(format!("dns-result-{generation}.json")),
        )
        .await;
    }
    fs::write(directory.path().join("exit-gate"), b"finish").unwrap();
    let output = finish(&run).await;
    assert_eq!(output["exit_code"], 0, "{output}");
    pinned.assert_all_stopped();
    engine.close().await.unwrap();
    server.positive_control(&format!("after-{nonce}.ww-tree.test."));
    for generation in 0..3 {
        let name = format!("sandbox-{generation}-{nonce}.ww-tree.test.");
        let result = read_json(
            directory
                .path()
                .join(format!("dns-result-{generation}.json")),
        );
        assert_eq!(
            server.arrivals(&name),
            0,
            "DnsQueryEx transmitted the sandbox nonce through a DNS service or direct socket despite network denial: generation {generation}, result {result}"
        );
        assert_ne!(
            result["status"], 0,
            "DNS unexpectedly resolved the fresh nonce without network authority"
        );
    }
    println!("NATIVE_TREE_DNS_BROKER_DENIED_OK");
}

async fn boundary_checks(fixture: &Fixture) {
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
    let directory = fixture.case("boundaries");
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("host-only.txt");
    fs::write(&secret, b"synthetic host control").unwrap();
    assert_eq!(fs::read(&secret).unwrap(), b"synthetic host control");
    fs::write(directory.path().join("allowed.txt"), b"allowed workspace").unwrap();
    let v4 = TcpListener::bind("127.0.0.1:0").unwrap();
    let v6 = TcpListener::bind("[::1]:0").unwrap();
    let external: SocketAddr = std::env::var("WW_WINDOWS_PROCESS_TREE_EXTERNAL_ENDPOINT").expect("Supply an explicitly controlled reachable non-loopback TCP endpoint for external-network qualification").parse().unwrap();
    assert!(!external.ip().is_loopback() && !external.ip().is_unspecified());
    let endpoints = [v4.local_addr().unwrap(), v6.local_addr().unwrap(), external];
    for endpoint in endpoints {
        TcpStream::connect_timeout(&endpoint, Duration::from_secs(3)).unwrap();
    }
    let registry = RegistryFixture::new();
    let registry_path = format!(
        "{}\\{}",
        fixture.provision["host_sid"].as_str().unwrap(),
        registry.0
    );
    for access in [KEY_READ, KEY_WRITE] {
        assert!(registry_open(HKEY_USERS, &registry_path, access));
    }
    let host_file = fs::File::open(&secret).unwrap();
    assert_ne!(
        unsafe {
            SetHandleInformation(
                host_file.as_raw_handle(),
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            )
        },
        0
    );
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };
    let mut identity: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    assert_ne!(
        unsafe {
            GetFileInformationByHandleEx(
                host_file.as_raw_handle(),
                FileIdInfo,
                (&mut identity as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        },
        0
    );
    let pipe_name = format!(r"\\.\pipe\ww-tree-control-{}", uuid::Uuid::new_v4());
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_name)
        .unwrap();
    let control = ClientOptions::new().open(&pipe_name).unwrap();
    server.connect().await.unwrap();
    drop(control);
    server.disconnect().unwrap();
    let waiting = server.connect();
    tokio::pin!(waiting);
    tokio::select! { result = &mut waiting => panic!("Unexpected host pipe connection {result:?}"), _ = tokio::time::sleep(Duration::from_millis(10)) => {} }
    write_json(
        directory.path().join("plan.json"),
        json!({"mode":"boundaries","outside":secret,"pipe":pipe_name,"registry":registry_path,"host_handle":host_file.as_raw_handle() as usize as u64,"host_volume":identity.VolumeSerialNumber,"host_file_id":identity.FileId.Identifier,"endpoints":endpoints.map(|e|e.to_string())}),
    );
    let (engine, run, _) = launch(
        fixture,
        directory.path(),
        fixture.arguments(directory.path()),
        Mode::Run,
    )
    .await;
    for generation in 0..3 {
        wait_for(directory.path().join(format!("receipt-{generation}.json"))).await;
    }
    let pinned = fixture.snapshot(directory.path(), 4);
    fs::write(directory.path().join("boundary-gate"), b"finish").unwrap();
    let output = finish(&run).await;
    assert_eq!(output["exit_code"], 0, "{output}");
    for generation in 0..3 {
        assert!(
            output["stdout"]
                .as_str()
                .unwrap()
                .contains(&format!("BOUNDARIES_OK_{generation}"))
        );
    }
    pinned.assert_all_stopped();
    engine.close().await.unwrap();
    // Denials are accepted only while the same targets remain host-accessible.
    assert_eq!(fs::read(secret).unwrap(), b"synthetic host control");
    for endpoint in endpoints {
        TcpStream::connect_timeout(&endpoint, Duration::from_secs(3)).unwrap();
    }
    let _control = ClientOptions::new().open(&pipe_name).unwrap();
    waiting.await.unwrap();
    for access in [KEY_READ, KEY_WRITE] {
        assert!(registry_open(HKEY_USERS, &registry_path, access));
    }
    println!("NATIVE_TREE_FILE_REGISTRY_NETWORK_IPC_HANDLES_OK");
}

async fn inbound_loopback_checks(fixture: &Fixture) {
    let directory = fixture.case("inbound");
    let v4 = TcpListener::bind("127.0.0.1:0").unwrap();
    let v6 = TcpListener::bind("[::1]:0").unwrap();
    let addresses = [v4.local_addr().unwrap(), v6.local_addr().unwrap()];
    for address in addresses {
        TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    }
    drop((v4, v6));
    write_json(
        directory.path().join("plan.json"),
        json!({"mode":"inbound","addresses":addresses}),
    );
    let (engine, run, _) = launch(
        fixture,
        directory.path(),
        fixture.arguments(directory.path()),
        Mode::Run,
    )
    .await;
    wait_for(directory.path().join("listeners.json")).await;
    let pinned = fixture.snapshot(directory.path(), 2);
    let observations = read_json(directory.path().join("listeners.json"));
    assert_eq!(observations.as_array().unwrap().len(), 2);
    for (address, observation) in addresses.iter().zip(observations.as_array().unwrap()) {
        assert_eq!(observation["address"], address.to_string());
        if observation["bound"] == true {
            assert!(
                TcpStream::connect_timeout(address, Duration::from_millis(750)).is_err(),
                "Inbound loopback escaped the account filter: {address}"
            );
        } else {
            assert_eq!(
                observation["permission_denied"], true,
                "A failed bind unrelated to policy is not isolation evidence: {observation}"
            );
        }
    }
    fs::write(directory.path().join("inbound-gate"), b"finish").unwrap();
    let output = finish(&run).await;
    assert_eq!(output["exit_code"], 0, "{output}");
    pinned.assert_all_stopped();
    engine.close().await.unwrap();
    for address in addresses {
        let listener = TcpListener::bind(address).unwrap();
        TcpStream::connect_timeout(&listener.local_addr().unwrap(), Duration::from_secs(3))
            .unwrap();
    }
    println!("NATIVE_TREE_INBOUND_LOOPBACK_OK");
}

async fn resource_checks(fixture: &mut Fixture) {
    for mode in ["memory", "cpu"] {
        let directory = fixture.case(mode);
        fixture.sandbox.limits = ProcessLimits {
            processes: 4,
            timeout_ms: 20_000,
            memory_bytes: if mode == "memory" {
                128 << 20
            } else {
                512 << 20
            },
            cpu_seconds: if mode == "cpu" { 1 } else { 10 },
            ..Default::default()
        };
        if mode == "memory" {
            write_json(
                directory.path().join("plan.json"),
                json!({"mode":"memory-control"}),
            );
            let control = Command::new(std::env::current_exe().unwrap())
                .args(payload_args())
                .current_dir(directory.path())
                .output()
                .unwrap();
            assert!(
                control.status.success(),
                "Host allocation control failed: {control:?}"
            );
            assert!(String::from_utf8_lossy(&control.stdout).contains("MEMORY_CONTROL_OK"));
            write_json(directory.path().join("plan.json"), json!({"mode":"memory"}));
        }
        let (engine, run, _) = launch(
            fixture,
            directory.path(),
            fixture.arguments(directory.path()),
            Mode::Run,
        )
        .await;
        wait_for(directory.path().join("resource-ready")).await;
        let pinned = fixture.snapshot(directory.path(), 2);
        fs::write(directory.path().join("resource-gate"), b"go").unwrap();
        let output = finish(&run).await;
        assert_eq!(
            output["timed_out"], false,
            "Resource test reached host wall timeout instead of native limit: {output}"
        );
        if mode == "memory" {
            assert_eq!(output["exit_code"], 0, "{output}");
            assert!(
                output["stdout"]
                    .as_str()
                    .unwrap()
                    .contains("MEMORY_LIMIT_OK")
            );
        } else {
            assert_ne!(
                output["exit_code"], 0,
                "Native aggregate Job CPU limit did not terminate the spinner"
            );
            assert!(output["stdout"].as_str().unwrap().contains("CPU_STARTED"));
        }
        pinned.assert_all_stopped();
        engine.close().await.unwrap();
        println!("NATIVE_TREE_RESOURCE_OK {mode}");
    }
}

async fn offline_cargo(fixture: &mut Fixture) {
    let directory = fixture.case("cargo");
    let cargo = PathBuf::from(
        std::env::var_os("WW_WINDOWS_PROCESS_TREE_CARGO")
            .expect("Supply the private, explicitly provisioned Cargo executable"),
    );
    let roots: Vec<PathBuf> =
        serde_json::from_value(fixture.provision["toolchain_roots"].clone()).unwrap();
    let cargo = cargo.canonicalize().unwrap();
    let provisioned = |path: &Path| {
        roots
            .iter()
            .any(|root| path.starts_with(root.canonicalize().unwrap()))
    };
    assert!(
        provisioned(&cargo),
        "Cargo must be inside a host-provisioned read-only toolchain root"
    );
    let environment = &fixture.provision["toolchain_environment"];
    let rustc = PathBuf::from(
        environment["RUSTC"]
            .as_str()
            .expect("Provision RUSTC explicitly"),
    )
    .canonicalize()
    .unwrap();
    assert!(provisioned(&rustc), "RUSTC must be a provisioned compiler");
    let linker = std::env::split_paths(environment["PATH"].as_str().unwrap())
        .filter_map(|directory| directory.join("link.exe").canonicalize().ok())
        .find(|path| provisioned(path))
        .expect("Provision the native MSVC link.exe explicitly");
    fs::create_dir(directory.path().join(".cargo")).unwrap();
    fs::write(
        directory.path().join(".cargo/config.toml"),
        format!(
            "[target.x86_64-pc-windows-msvc]\nlinker = {}\n",
            serde_json::to_string(&linker.to_string_lossy()).unwrap()
        ),
    )
    .unwrap();
    println!(
        "NATIVE_TREE_TOOLCHAIN {}",
        json!({"cargo":cargo,"rustc":rustc,"linker":linker})
    );
    fs::create_dir(directory.path().join("src")).unwrap();
    fs::write(directory.path().join("Cargo.toml"), "[package]\nname = \"tree-offline-acceptance\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n").unwrap();
    fs::write(directory.path().join("src/lib.rs"), "#[test] fn native_behavior() { assert_eq!(2 + 3, 5); std::fs::write(\"native-artifact.txt\", b\"built and tested by provisioned Rust/MSVC\\n\").unwrap(); println!(\"OFFLINE_NATIVE_CARGO_OK\"); }\n").unwrap();
    fixture.sandbox.limits = ProcessLimits {
        timeout_ms: 120_000,
        cpu_seconds: 120,
        memory_bytes: 2 << 30,
        processes: 32,
        ..Default::default()
    };
    let arguments = json!({"argv":[cargo,"test","--offline","--","--nocapture"],"cwd":directory.path().strip_prefix(&fixture.workspace.path).unwrap().to_string_lossy()});
    let (engine, run, _) = launch(fixture, directory.path(), arguments, Mode::Run).await;
    let output = finish(&run).await;
    assert_eq!(
        output["exit_code"], 0,
        "Private Rust/MSVC offline Cargo did not pass: {output}"
    );
    assert!(
        output["stdout"]
            .as_str()
            .unwrap()
            .contains("OFFLINE_NATIVE_CARGO_OK")
    );
    assert_eq!(
        fs::read(directory.path().join("native-artifact.txt")).unwrap(),
        b"built and tested by provisioned Rust/MSVC\n"
    );
    assert!(
        fs::read_dir(directory.path().join("target/debug/deps"))
            .unwrap()
            .map(|entry| entry.unwrap())
            .any(|entry| entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "exe")
                && entry.metadata().unwrap().len() > 0),
        "Cargo must leave a native Windows test executable in the workspace"
    );
    engine.close().await.unwrap();
    println!("NATIVE_TREE_CARGO_OUTPUT {output}");
    println!("NATIVE_TREE_OFFLINE_CARGO_OK");
}

#[test]
#[ignore = "native sandbox payload, launched only by the qualification harness"]
fn process_tree_payload() {
    let plan = read_json("plan.json");
    let generation: usize = std::env::var("WW_TREE_GENERATION")
        .ok()
        .map(|v| v.parse().unwrap())
        .unwrap_or(0);
    if plan["mode"] == "restart-must-not-run" {
        fs::write("restart-executed", b"unexpected").unwrap();
        return;
    }
    if plan["mode"] == "inbound" {
        let addresses: Vec<SocketAddr> = serde_json::from_value(plan["addresses"].clone()).unwrap();
        let mut listeners = Vec::new();
        let mut observations = Vec::new();
        for address in addresses {
            match TcpListener::bind(address) {
                Ok(listener) => { listeners.push(listener); observations.push(json!({"address":address.to_string(),"bound":true})); },
                Err(error) => observations.push(json!({"address":address.to_string(),"bound":false,"permission_denied":error.kind()==std::io::ErrorKind::PermissionDenied,"os_error":error.raw_os_error()})),
            }
        }
        write_json("listeners.json", json!(observations));
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !Path::new("inbound-gate").is_file() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(listeners);
        println!("INBOUND_LISTENERS_CHECKED");
        return;
    }
    if plan["mode"] == "memory-control" {
        let mut allocation = Vec::<u8>::new();
        allocation
            .try_reserve_exact(256 << 20)
            .expect("Host allocation control");
        println!("MEMORY_CONTROL_OK");
        return;
    }
    if plan["mode"] == "memory" || plan["mode"] == "cpu" {
        fs::write("resource-ready", b"started").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !Path::new("resource-gate").is_file() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        if plan["mode"] == "memory" {
            let mut allocation = Vec::<u8>::new();
            assert!(
                allocation.try_reserve_exact(256 << 20).is_err(),
                "Native Job memory budget was not enforced"
            );
            println!("MEMORY_LIMIT_OK");
            return;
        }
        println!("CPU_STARTED");
        std::io::stdout().flush().unwrap();
        let mut value = 1u64;
        loop {
            value = std::hint::black_box(value.wrapping_mul(1664525).wrapping_add(1013904223));
        }
    }
    if generation == 0
        && let Some(expected) = plan["stdin"].as_str()
    {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input).unwrap();
        assert_eq!(
            input, expected,
            "Stdin did not cross both explicit handle lists"
        );
    }
    if plan["mode"] == "boundaries" {
        let mut descendant = if generation < 2 {
            Some(Host(
                Command::new(std::env::current_exe().unwrap())
                    .args(payload_args())
                    .env("WW_TREE_GENERATION", (generation + 1).to_string())
                    .creation_flags(DETACHED_PROCESS)
                    .spawn()
                    .unwrap(),
            ))
        } else {
            None
        };
        assert_eq!(fs::read("allowed.txt").unwrap(), b"allowed workspace");
        fs::write("created.txt", b"workspace write").unwrap();
        assert!(fs::read(plan["outside"].as_str().unwrap()).is_err());
        assert!(
            fs::OpenOptions::new()
                .write(true)
                .open(plan["outside"].as_str().unwrap())
                .is_err()
        );
        for endpoint in plan["endpoints"].as_array().unwrap() {
            let endpoint: SocketAddr = endpoint.as_str().unwrap().parse().unwrap();
            assert!(
                TcpStream::connect_timeout(&endpoint, Duration::from_millis(750)).is_err(),
                "Network escaped: {endpoint}"
            );
        }
        assert!(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(plan["pipe"].as_str().unwrap())
                .is_err()
        );
        for access in [KEY_READ, KEY_WRITE] {
            assert!(
                !registry_open(HKEY_USERS, plan["registry"].as_str().unwrap(), access),
                "Host registry key escaped the dedicated account boundary"
            );
        }
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
        };
        let mut identity: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        let inherited = unsafe {
            GetFileInformationByHandleEx(
                plan["host_handle"].as_u64().unwrap() as usize as HANDLE,
                FileIdInfo,
                (&mut identity as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        assert!(
            inherited == 0
                || plan["host_volume"] != identity.VolumeSerialNumber
                || plan["host_file_id"] != json!(identity.FileId.Identifier),
            "Unlisted inheritable host handle crossed the launch boundary"
        );
        let mut in_job = 0;
        assert_ne!(
            unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) },
            0
        );
        let escaped = Command::new(std::env::current_exe().unwrap())
            .args(payload_args())
            .env("WW_TREE_GENERATION", "99")
            .creation_flags(CREATE_BREAKAWAY_FROM_JOB | DETACHED_PROCESS)
            .spawn();
        if let Ok(mut escaped) = escaped {
            let _ = escaped.kill();
            let _ = escaped.wait();
            panic!("Boundary payload broke away");
        }
        write_json(
            format!("receipt-{generation}.json"),
            json!({"pid":std::process::id(),"created":birth(unsafe { GetCurrentProcess() }),"generation":generation,"in_job":in_job != 0,"breakaway_denied":true}),
        );
        println!("BOUNDARIES_OK_{generation}");
        std::io::stdout().flush().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !Path::new("boundary-gate").is_file() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(child) = &mut descendant {
            assert!(child.0.wait().unwrap().success());
        }
        return;
    }
    let mut in_job = 0;
    assert_ne!(
        unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) },
        0
    );
    let mut escaped = Command::new(std::env::current_exe().unwrap());
    escaped
        .args(payload_args())
        .env("WW_TREE_GENERATION", "99")
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB | DETACHED_PROCESS);
    let breakaway = escaped.spawn();
    if let Ok(mut escaped) = breakaway {
        let _ = escaped.kill();
        let _ = escaped.wait();
        panic!("Breakaway child escaped");
    }
    let mut child = if generation < 2 {
        Some(Host(
            Command::new(std::env::current_exe().unwrap())
                .args(payload_args())
                .env("WW_TREE_GENERATION", (generation + 1).to_string())
                .creation_flags(DETACHED_PROCESS)
                .spawn()
                .unwrap(),
        ))
    } else {
        None
    };
    write_json(
        format!("receipt-{generation}.json"),
        json!({"pid":std::process::id(),"created":birth(unsafe { GetCurrentProcess() }),"generation":generation,"in_job":in_job != 0,"breakaway_denied":true}),
    );
    println!("TREE_GENERATION_{generation}");
    std::io::stdout().flush().unwrap();
    if generation == 0 {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !Path::new("receipt-2.json").is_file() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        // At cap four the trusted runner and three generations already fill
        // the Job. Headless creation must not add implicit console helpers.
        let extra = Command::new(std::env::current_exe().unwrap())
            .args(payload_args())
            .env("WW_TREE_GENERATION", "3")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        let denied = match extra {
            Err(_) => true,
            Ok(mut process) => {
                let _ = process.kill();
                let _ = process.wait();
                false
            }
        };
        write_json("budget.json", json!({"extra_child_denied":denied}));
    }
    if plan["mode"] == "dns" {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !Path::new("dns-gate").is_file() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let server: SocketAddr = serde_json::from_value(plan["dns_server"].clone()).unwrap();
        let name = format!(
            "sandbox-{generation}-{}.ww-tree.test.",
            plan["dns_nonce"].as_str().unwrap()
        );
        let (status, record) = tree_dns::query(server, &name);
        write_json(
            format!("dns-result-{generation}.json"),
            json!({"status":status,"record":record}),
        );
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !Path::new("exit-gate").is_file() {
        assert!(
            std::time::Instant::now() < deadline,
            "Host gate never opened"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    if let Some(child) = &mut child {
        assert!(child.0.wait().unwrap().success());
    }
}
