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
        Arc, Mutex,
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
    Security::{Authorization::ConvertSidToStringSidW, *},
    System::{
        Console::*,
        Diagnostics::ToolHelp::*,
        JobObjects::IsProcessInJob,
        Registry::*,
        StationsAndDesktops::*,
        SystemServices::{ACCESS_ALLOWED_ACE_TYPE, SE_GROUP_ENABLED, SE_GROUP_LOGON_ID},
        Threading::*,
    },
    UI::WindowsAndMessaging::*,
};

#[path = "fixtures/windows_tree_dns.rs"]
mod tree_dns;
#[path = "fixtures/windows_tree_network.rs"]
mod tree_network;
#[path = "fixtures/windows_tree_ui.rs"]
mod tree_ui;

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
    fn assert_all_stopped_now(&self) {
        for handle in &self.0 {
            assert_eq!(
                unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) },
                WAIT_OBJECT_0,
                "Cleanup returned while a pinned Job member was still alive"
            );
        }
    }
    fn wait_after_abrupt_host_death(&self) {
        // Host termination has no cleanup-return promise. Kill-on-close may
        // complete asynchronously, so only this separately labelled case waits.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        for handle in &self.0 {
            let remaining = deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_millis() as u32;
            assert_eq!(
                unsafe { WaitForSingleObject(handle.as_raw_handle(), remaining) },
                WAIT_OBJECT_0,
                "Kill-on-close did not stop a retained member after abrupt host death"
            );
        }
        self.assert_all_stopped_now();
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
        let description = sandbox
            .execution_description(&workspace, true)
            .await
            .unwrap();
        assert_eq!(provision["schema_version"], 3);
        let weight = u16::try_from(
            provision["network_sublayer_weight"]
                .as_u64()
                .expect("Provisioning must bind the actual WFP sublayer weight"),
        )
        .unwrap();
        assert_eq!(description["network_sublayer_weight"], weight);
        assert_eq!(
            description["network_sublayer_key"],
            provision["network_sublayer_key"]
        );

        // Exercise the real ACL/account/WFP validation with one altered receipt
        // field. The sibling inherits the protected provisioning directory ACL;
        // leave the original receipt and installed WFP state untouched.
        let original_bytes = fs::read(&manifest).unwrap();
        let changed_manifest = tempfile::Builder::new()
            .prefix("ww-tree-weight-mismatch-")
            .suffix(".json")
            .tempfile_in(manifest.parent().unwrap())
            .unwrap()
            .into_temp_path();
        // into_temp_path closes the creation handle; fs::write also closes its
        // handle before load pins trusted files with restrictive sharing modes.
        let mut changed = provision.clone();
        changed["network_sublayer_weight"] = json!(weight ^ 1);
        fs::write(&changed_manifest, serde_json::to_vec(&changed).unwrap()).unwrap();
        let mut changed_sandbox = sandbox.clone();
        changed_sandbox
            .native
            .windows_process_tree
            .as_mut()
            .unwrap()
            .provisioning = changed_manifest.to_path_buf();
        let error = changed_sandbox
            .execution_description(&workspace, true)
            .await
            .expect_err("A changed WFP sublayer weight must prevent profile selection");
        assert_eq!(error.code, "sandbox_unavailable");
        assert!(
            error
                .message
                .starts_with("Required network sublayer weight changed"),
            "Expected the specific WFP readback mismatch, got {error}"
        );
        assert_eq!(fs::read(&manifest).unwrap(), original_bytes);
        changed_manifest.close().unwrap();
        println!(
            "NATIVE_TREE_WFP_WEIGHT_RECEIPT_OK {}",
            json!({"effective_weight":weight,"rejected_weight":weight ^ 1})
        );
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
        let ids: Vec<u32> = serde_json::from_value(snapshot["process_ids"].clone()).unwrap();
        let mut pinned = Pinned(Vec::with_capacity(ids.len()));
        let mut observations = Vec::with_capacity(ids.len());
        for pid in &ids {
            let handle = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                    0,
                    *pid,
                )
            };
            if handle.is_null() {
                let error = unsafe { GetLastError() };
                observations.push(json!({"pid":pid,"open_error":error}));
                continue;
            }
            pinned
                .0
                .push(unsafe { OwnedHandle::from_raw_handle(handle) });
            let wait = unsafe { WaitForSingleObject(handle, 0) };
            let wait_error = (wait == WAIT_FAILED).then(|| unsafe { GetLastError() });
            let mut observed = json!({"pid":pid,"wait_timeout_ms":0,"wait_result":wait});
            if let Some(error) = wait_error {
                observed["wait_error"] = json!(error);
            }
            observations.push(observed);
        }
        // Pin and zero-time-check every reported PID before metadata queries or
        // log/file output introduce additional delay into the observation.
        for (handle, observed) in pinned.0.iter().zip(
            observations
                .iter_mut()
                .filter(|value| value.get("wait_result").is_some()),
        ) {
            let handle = handle.as_raw_handle();
            let mut created: FILETIME = unsafe { std::mem::zeroed() };
            let mut exited = created;
            let mut kernel = created;
            let mut user = created;
            if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
                == 0
            {
                observed["times_error"] = json!(unsafe { GetLastError() });
            } else {
                observed["created"] =
                    json!(((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64);
                observed["exited"] =
                    json!(((exited.dwHighDateTime as u64) << 32) | exited.dwLowDateTime as u64);
            }
            let mut path = vec![0u16; 32768];
            let mut length = path.len() as u32;
            if unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut length) } == 0
            {
                observed["image_error"] = json!(unsafe { GetLastError() });
            } else {
                observed["image"] = json!(String::from_utf16_lossy(&path[..length as usize]));
            }
            let mut exit_code = 0;
            if unsafe { GetExitCodeProcess(handle, &mut exit_code) } == 0 {
                observed["exit_code_error"] = json!(unsafe { GetLastError() });
            } else {
                observed["exit_code"] = json!(exit_code);
            }
        }
        // Emit the complete unfiltered report before any expectations can panic.
        // The native --nocapture log survives fixture directory cleanup.
        println!("NATIVE_TREE_JOB_SNAPSHOT {snapshot}");
        let diagnostics =
            json!({"snapshot":snapshot,"expected_members":expected,"observations":observations});
        println!("NATIVE_TREE_JOB_MEMBER_DIAGNOSTICS {diagnostics}");
        write_json(
            directory.join("host-snapshot-diagnostics.json"),
            diagnostics,
        );
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
        assert_eq!(snapshot["limits"]["ui_restrictions"], 0xff, "{snapshot}");
        assert_eq!(
            ids.iter().collect::<BTreeSet<_>>().len(),
            expected,
            "Unexpected unique Job members: {snapshot}; observations: {observations:?}"
        );
        assert_eq!(
            ids.len(),
            expected,
            "Trusted runner plus all payload generations must be counted: {snapshot}"
        );
        assert_eq!(snapshot["active_processes"], expected, "{snapshot}");
        assert_eq!(
            pinned.0.len(),
            ids.len(),
            "Cannot pin every Job member: {observations:?}"
        );
        for observation in &observations {
            assert_eq!(
                observation["wait_result"], WAIT_TIMEOUT,
                "Fixture exited before pinning: {observation}"
            );
        }
        if expected == 4 {
            pinned.assert_receipts(&ids, directory);
        }
        // This receipt is also the abrupt-host-death synchronization gate. Do
        // not publish it until every limit, member and live-handle check passed.
        write_json(directory.join("host-snapshot.json"), snapshot);
        pinned
    }
    async fn snapshot_with_cleanup(
        &self,
        directory: &Path,
        expected: usize,
        run: &RunHandle,
        engine: &WorkerEngine,
    ) -> Pinned {
        self.assert_with_cleanup(run, engine, || self.snapshot(directory, expected))
            .await
    }
    async fn assert_with_cleanup<T>(
        &self,
        run: &RunHandle,
        engine: &WorkerEngine,
        check: impl FnOnce() -> T,
    ) -> T {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(check)) {
            Ok(value) => value,
            Err(original_panic) => {
                // The original observation has already failed. Keep the Tokio
                // runtime alive while the real supervisor confirms cleanup;
                // cleanup success must never turn that assertion into a pass.
                let cancel_error = run
                    .cancel(
                        "Native snapshot assertion failed",
                        "snapshot-failure-cleanup",
                    )
                    .await
                    .err()
                    .map(|error| error.to_string());
                let close_error = engine.close().await.err().map(|error| error.to_string());
                println!(
                    "NATIVE_TREE_SNAPSHOT_FAILURE_CLEANUP {}",
                    json!({"cancel_error":cancel_error,"close_error":close_error,"cleanup_confirmed":close_error.is_none()})
                );
                std::panic::resume_unwind(original_panic);
            }
        }
    }
}
async fn wait_for(path: impl AsRef<Path>) {
    wait_for_within(path, Duration::from_secs(15)).await;
}
async fn wait_for_within(path: impl AsRef<Path>, timeout: Duration) {
    let path = path.as_ref();
    tokio::time::timeout(timeout, async {
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
#[derive(Default)]
struct CompletionProbe {
    handles: Mutex<Vec<OwnedHandle>>,
    observations: Mutex<Vec<(String, Vec<u32>)>>,
    results: Mutex<Vec<(String, ToolResult)>>,
}
impl CompletionProbe {
    fn pin(&self, pinned: &Pinned) {
        let mut handles = self.handles.lock().unwrap();
        assert!(handles.is_empty(), "Pin each invocation only once");
        *handles = pinned
            .0
            .iter()
            .map(|handle| handle.try_clone().unwrap())
            .collect();
    }
    fn assert_completed_at_tool_return(&self) {
        let observations = self.observations.lock().unwrap();
        assert!(
            !observations.is_empty(),
            "No completed tool return was observed"
        );
        for (tool, states) in observations.iter() {
            assert!(
                !states.is_empty(),
                "Completion observation requires retained handles"
            );
            assert!(
                states.iter().all(|state| *state == WAIT_OBJECT_0),
                "{tool} returned completion while retained process handles were not signaled: {states:?}"
            );
            println!(
                "NATIVE_TREE_IMMEDIATE_RETURN_OK {}",
                json!({"tool":tool,"wait_timeout_ms":0,"handle_states":states})
            );
        }
    }
}
struct ObserveCompletion {
    inner: Arc<dyn Tool>,
    probe: Arc<CompletionProbe>,
}
#[async_trait::async_trait]
impl Tool for ObserveCompletion {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }
    async fn prepare(&self, args: Value, context: &ToolContext) -> Result<Value> {
        self.inner.prepare(args, context).await
    }
    async fn execute(&self, prepared: Value, context: &ToolContext) -> Result<ToolResult> {
        let name = self.inner.definition().name;
        let result = self.inner.execute(prepared, context).await;
        // Inspect the original retained process handles immediately at the
        // builtin's return, before another model turn, file call or Engine close.
        let states: Vec<_> = self
            .probe
            .handles
            .lock()
            .unwrap()
            .iter()
            .map(|handle| unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) })
            .collect();
        let completed = name == "process.run"
            || result
                .as_ref()
                .is_ok_and(|result| result.data["status"] == "finished");
        if completed {
            // Retain the builtin result only after the immediate handle sample.
            // Engine persistence may replace large output with an artifact link.
            if let Ok(result) = &result {
                self.probe
                    .results
                    .lock()
                    .unwrap()
                    .push((name.clone(), result.clone()));
            }
            println!(
                "NATIVE_TREE_TOOL_RETURN {}",
                json!({"tool":name,"result":result})
            );
        }
        if completed && !states.is_empty() {
            self.probe.observations.lock().unwrap().push((name, states));
        }
        result
    }
}
struct Provider {
    step: AtomicUsize,
    arguments: Value,
    source: String,
    mode: Mode,
    observed: tokio::sync::Notify,
    release: tokio::sync::Notify,
    completion: Arc<CompletionProbe>,
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
            .replace('\\', "/"),
        mode,
        observed: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        completion: Arc::new(CompletionProbe::default()),
    });
    let builtins = ToolRegistry::builtins().unwrap();
    let mut tools = ToolRegistry::default();
    for name in profile().tools {
        let inner = builtins.get(&name).unwrap();
        if matches!(
            name.as_str(),
            "process.run" | "process.status" | "process.cancel" | "process.wait"
        ) {
            tools
                .register(Arc::new(ObserveCompletion {
                    inner,
                    probe: provider.completion.clone(),
                }))
                .unwrap();
        } else {
            tools.register(inner).unwrap();
        }
    }
    let engine = WorkerEngine::builder(provider.clone())
        .tools(tools)
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
async fn finish(run: &RunHandle, provider: &Provider) -> Value {
    let terminal = tokio::time::timeout(Duration::from_secs(150), run.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(terminal.status, RunStatus::Completed, "{terminal:?}");
    provider.completion.assert_completed_at_tool_return();
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
        "Workspace read after confirmed cleanup failed: {reads:?}"
    );
    let invocation = state
        .invocations
        .iter()
        .find(|i| i.call.name == "process.run" || i.call.name == "process.wait")
        .unwrap();
    let persisted = invocation.result.as_ref().unwrap();
    assert_eq!(persisted.status, Outcome::Succeeded, "{persisted:?}");
    let results = provider.completion.results.lock().unwrap();
    let (name, actual) = results
        .iter()
        .find(|(name, _)| *name == invocation.call.name)
        .expect("Completed invocation must retain its actual builtin result");
    assert_eq!(actual.status, Outcome::Succeeded, "{actual:?}");
    if name == "process.wait" {
        assert_eq!(actual.data["status"], "finished", "{actual:?}");
        actual.data["output"].clone()
    } else {
        actual.data.clone()
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
    for scenario in [
        "normal",
        "parent-exit",
        "timeout",
        "cancel",
        "abort",
        "engine-close",
    ] {
        let payload_mode = if scenario == "parent-exit" {
            "parent-exit"
        } else {
            "tree"
        };
        let directory = fixture.case(payload_mode);
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
                json!({"mode":payload_mode,"stdin":input}),
            );
            arguments["stdin"] = json!(input);
        }
        let (engine, run, provider) = launch(&fixture, directory.path(), arguments, mode).await;
        for generation in 0..3 {
            wait_for(directory.path().join(format!("receipt-{generation}.json"))).await;
        }
        wait_for(directory.path().join("budget.json")).await;
        let pinned = fixture
            .snapshot_with_cleanup(directory.path(), 4, &run, &engine)
            .await;
        provider.completion.pin(&pinned);
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
            "parent-exit" => fs::write(
                directory.path().join("parent-exit-gate"),
                b"finish parent only",
            )
            .unwrap(),
            "cancel" => provider.release.notify_one(),
            "abort" => {
                run.cancel("qualification abort", "abort").await.unwrap();
            }
            "engine-close" => {
                engine.close().await.unwrap();
                pinned.assert_all_stopped_now();
            }
            _ => {}
        }
        if matches!(scenario, "abort" | "engine-close") {
            tokio::time::timeout(Duration::from_secs(15), run.wait())
                .await
                .unwrap()
                .unwrap();
            pinned.assert_all_stopped_now();
        } else {
            let output = finish(&run, &provider).await;
            match scenario {
                "normal" | "parent-exit" => {
                    assert_eq!(output["exit_code"], 0);
                    assert_eq!(output["timed_out"], false);
                    assert_eq!(output["cancelled"], false);
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
        if scenario == "parent-exit" {
            let before_exit = read_json(directory.path().join("parent-exit-observation.json"));
            assert_eq!(before_exit["child_state"], WAIT_TIMEOUT);
            assert_eq!(before_exit["grandchild_state"], WAIT_TIMEOUT);
            assert_eq!(
                before_exit["child_pid"],
                read_json(directory.path().join("receipt-1.json"))["pid"]
            );
            assert_eq!(
                before_exit["grandchild_pid"],
                read_json(directory.path().join("receipt-2.json"))["pid"]
            );
            assert!(
                !directory.path().join("exit-gate").exists(),
                "Descendants must stay gated while the parent exits"
            );
            println!("NATIVE_TREE_PARENT_EXIT_LIVE_DESCENDANTS {before_exit}");
        }
        // The OS handles are checked after the tool/engine cleanup promise.
        pinned.assert_all_stopped_now();
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
    console_budget_checks(&mut fixture).await;
    ui_boundary_checks(&mut fixture).await;
    dns_broker_checks(&fixture).await;
    thread_identity_network_checks(&mut fixture).await;
    boundary_checks(&fixture).await;
    inbound_loopback_checks(&fixture).await;
    resource_checks(&mut fixture).await;
    bounded_output_checks(&mut fixture).await;
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

// Only console-helper candidates are enumerated globally. Existing protected
// processes need no token access; new console helpers cannot be ignored merely
// because they did not appear in the owned Job's membership list.
fn console_helpers() -> Vec<(u32, u32, String)> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    assert_ne!(snapshot, INVALID_HANDLE_VALUE);
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut found = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) };
    let mut helpers = Vec::new();
    while found != 0 {
        let length = entry
            .szExeFile
            .iter()
            .position(|value| *value == 0)
            .unwrap();
        let image = String::from_utf16_lossy(&entry.szExeFile[..length]);
        if image.eq_ignore_ascii_case("conhost.exe")
            || image.eq_ignore_ascii_case("OpenConsole.exe")
        {
            helpers.push((entry.th32ProcessID, entry.th32ParentProcessID, image));
        }
        found = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) };
    }
    assert_eq!(unsafe { GetLastError() }, ERROR_NO_MORE_FILES);
    helpers
}
fn process_user_sid(process: HANDLE) -> std::io::Result<String> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut size = 0;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut size,
        )
    };
    if size == 0 || size > 65536 {
        return Err(std::io::Error::other(
            "Invalid console-helper token information size",
        ));
    }
    let mut user = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            user.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut length = 0;
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
    }
    let result = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    unsafe {
        LocalFree(text.cast());
    }
    Ok(result)
}
fn new_console_helpers(before: &BTreeSet<u32>) -> Vec<Value> {
    console_helpers()
        .into_iter()
        .filter(|(pid, _, _)| !before.contains(pid))
        .map(|(pid, parent, image)| {
            let mut observed = json!({"pid":pid,"parent_pid":parent,"image":image});
            let handle = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    0,
                    pid,
                )
            };
            if handle.is_null() {
                let error = unsafe { GetLastError() };
                observed["query_error"] = json!(error);
                return observed;
            }
            let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
            let wait = unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) };
            let wait_error = (wait == WAIT_FAILED).then(|| unsafe { GetLastError() });
            observed["wait_result"] = json!(wait);
            observed["wait_timeout_ms"] = json!(0);
            if let Some(error) = wait_error {
                observed["query_error"] = json!(error);
            }
            observed["created"] = json!(birth(handle.as_raw_handle()));
            match process_user_sid(handle.as_raw_handle()) {
                Ok(sid) => observed["user_sid"] = json!(sid),
                Err(error) => observed["query_error"] = json!(error.to_string()),
            }
            observed
        })
        .collect()
}
const UI_CANARY_MESSAGE: u32 = tree_ui::MESSAGE;
fn user_object_name(handle: HANDLE) -> String {
    let mut buffer = vec![0u16; 1024];
    let mut required = 0;
    assert_ne!(
        unsafe {
            GetUserObjectInformationW(
                handle,
                UOI_NAME,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 2) as u32,
                &mut required,
            )
        },
        0,
        "GetUserObjectInformationW: {}",
        std::io::Error::last_os_error()
    );
    let end = buffer.iter().position(|c| *c == 0).unwrap();
    String::from_utf16(&buffer[..end]).unwrap()
}
fn current_desktop_name() -> String {
    unsafe {
        format!(
            "{}\\{}",
            user_object_name(GetProcessWindowStation()),
            user_object_name(GetThreadDesktop(GetCurrentThreadId()))
        )
    }
}
struct InputDesktopRecovery {
    original: HDESK,
    owned: Option<String>,
}
impl InputDesktopRecovery {
    fn restore_if_owned_input(&self) {
        if let Some(owned) = &self.owned {
            // A failed payload may have switched before writing its observation.
            // Inspect only object names, never change an unrelated user/UAC desktop.
            let current = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
            if !current.is_null() {
                let read_name = |handle| -> Option<String> {
                    let mut buffer = vec![0u16; 1024];
                    let mut required = 0;
                    if unsafe {
                        GetUserObjectInformationW(
                            handle,
                            UOI_NAME,
                            buffer.as_mut_ptr().cast(),
                            (buffer.len() * 2) as u32,
                            &mut required,
                        )
                    } == 0
                    {
                        return None;
                    }
                    let end = buffer.iter().position(|c| *c == 0)?;
                    String::from_utf16(&buffer[..end]).ok()
                };
                let current_name = read_name(unsafe { GetProcessWindowStation() })
                    .zip(read_name(current))
                    .map(|(station, desktop)| format!("{station}\\{desktop}"));
                unsafe {
                    CloseDesktop(current);
                }
                if current_name.as_ref() == Some(owned) {
                    let restored = unsafe { SwitchDesktop(self.original) };
                    if restored == 0 {
                        eprintln!(
                            "Failed to restore input desktop after UI-test failure: {}",
                            std::io::Error::last_os_error()
                        );
                    }
                }
            }
        }
    }
}
impl Drop for InputDesktopRecovery {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.restore_if_owned_input();
        }
        unsafe {
            CloseDesktop(self.original);
        }
    }
}
async fn ui_assert_with_cleanup<T>(
    fixture: &Fixture,
    run: &RunHandle,
    engine: &WorkerEngine,
    observer: &mut Host,
    check: impl FnOnce() -> T,
) -> T {
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(check));
    if checked.is_err() && observer.0.try_wait().ok().flatten().is_none() {
        let _ = observer.0.kill();
        let _ = observer.0.wait();
    }
    // Observer USER handles must be gone before trusted Job cleanup can close
    // the same private station. Preserve the original assertion failure.
    fixture
        .assert_with_cleanup(run, engine, || match checked {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        })
        .await
}
async fn ui_wait_with_cleanup(
    fixture: &Fixture,
    path: PathBuf,
    run: &RunHandle,
    engine: &WorkerEngine,
    observer: &mut Host,
) {
    use futures_util::FutureExt;
    let observed = std::panic::AssertUnwindSafe(wait_for(path))
        .catch_unwind()
        .await;
    ui_assert_with_cleanup(fixture, run, engine, observer, || {
        if let Err(panic) = observed {
            std::panic::resume_unwind(panic);
        }
    })
    .await;
}
#[test]
#[ignore = "ordinary host observer for the process-tree private USER-object tests"]
fn process_tree_ui_observer() {
    tree_ui::observer();
}
async fn ui_boundary_checks(fixture: &mut Fixture) {
    let original_limits = fixture.sandbox.limits.clone();
    fixture.sandbox.limits.processes = 2;
    let directory = fixture.case("ui");
    let host_desktop = current_desktop_name();
    let mut input = InputDesktopRecovery {
        original: unsafe { OpenInputDesktop(0, 0, DESKTOP_SWITCHDESKTOP) },
        owned: None,
    };
    assert!(!input.original.is_null());
    let (engine, run, provider) = launch(
        fixture,
        directory.path(),
        fixture.arguments(directory.path()),
        Mode::Run,
    )
    .await;
    fixture_file_with_cleanup(
        fixture,
        directory.path().join("ui-ready.json"),
        &run,
        &engine,
    )
    .await;
    let pinned = fixture
        .snapshot_with_cleanup(directory.path(), 2, &run, &engine)
        .await;
    provider.completion.pin(&pinned);
    // Obtain the expected identity directly from trusted runtime state, rather
    // than treating a writable workspace receipt as authority for clipboard I/O.
    let desktop = fixture
        .assert_with_cleanup(&run, &engine, || {
            let snapshot = fixture
                .sandbox
                .windows_process_tree_snapshot(&fixture.workspace)
                .unwrap();
            let desktop = snapshot["desktop"].as_str().unwrap().to_string();
            let (station, local) = desktop.split_once('\\').unwrap();
            assert!(local.starts_with("werk-tree-"));
            assert_ne!(station, host_desktop.split_once('\\').unwrap().0);
            assert_eq!(
                read_json(directory.path().join("ui-ready.json"))["desktop"],
                desktop
            );
            desktop
        })
        .await;
    input.owned = Some(desktop.clone());
    let mut observer = Host(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "process_tree_ui_observer",
                "--nocapture",
            ])
            .env("WW_TREE_UI_DIRECTORY", directory.path())
            .env("WW_TREE_UI_DESKTOP", &desktop)
            .env("WW_TREE_UI_NONCE", uuid::Uuid::new_v4().to_string())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    ui_wait_with_cleanup(
        fixture,
        directory.path().join("ui-observer-ready.json"),
        &run,
        &engine,
        &mut observer,
    )
    .await;
    let observer_pid = observer.0.id();
    let observer_birth = birth(observer.0.as_raw_handle());
    let ready = ui_assert_with_cleanup(fixture, &run, &engine, &mut observer, || {
        let ready = read_json(directory.path().join("ui-observer-ready.json"));
        assert_eq!(ready["pid"], observer_pid);
        assert_eq!(ready["created"], observer_birth);
        assert_eq!(ready["desktop"], desktop);
        assert_eq!(ready["private_station_noninteractive"], true);
        assert_eq!(ready["synthetic_clipboard_read_write_control"], true);
        let snapshot = fixture
            .sandbox
            .windows_process_tree_snapshot(&fixture.workspace)
            .unwrap();
        assert_eq!(snapshot["active_processes"], 2);
        assert!(
            !snapshot["process_ids"]
                .as_array()
                .unwrap()
                .contains(&json!(observer_pid))
        );
        assert_eq!(
            current_desktop_name(),
            host_desktop,
            "Worker host changed USER association"
        );
        ready
    })
    .await;
    write_json(
        directory.path().join("ui-request.json"),
        json!({
            "desktop":desktop,"window":ready["window"],"nonce":ready["nonce"],
            "clipboard_format":ready["clipboard_format"],
        }),
    );
    fs::write(directory.path().join("ui-attempt-gate"), b"attempt").unwrap();
    ui_wait_with_cleanup(
        fixture,
        directory.path().join("ui-attempt.json"),
        &run,
        &engine,
        &mut observer,
    )
    .await;
    fs::write(
        directory.path().join("ui-observer-check"),
        b"check and close",
    )
    .unwrap();
    ui_wait_with_cleanup(
        fixture,
        directory.path().join("ui-observer-result.json"),
        &run,
        &engine,
        &mut observer,
    )
    .await;
    let exited = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match observer.0.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Err(error) => break Err(error),
                Ok(None) => {}
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    ui_assert_with_cleanup(fixture, &run, &engine, &mut observer, || {
        assert!(
            exited
                .expect("Observer did not close its USER handles")
                .expect("Cannot observe host UI process exit")
                .success(),
            "Host UI observer failed"
        );
    })
    .await;
    // No external observer process/USER handle remains when the payload may end.
    fixture
        .assert_with_cleanup(&run, &engine, || {
            let attempt = read_json(directory.path().join("ui-attempt.json"));
            let observed = read_json(directory.path().join("ui-observer-result.json"));
            if attempt["switch_desktop"] == true {
                input.restore_if_owned_input();
            }
            println!(
                "NATIVE_TREE_UI_BOUNDARY_OBSERVATION {}",
                json!({
                    "host_chosen_desktop":desktop,"observer_ready":ready,"observer_after":observed,
                    "observer_exited_before_payload":true,"attempt":attempt,
                })
            );
            assert_eq!(observed["pid"], observer_pid);
            assert_eq!(observed["desktop"], desktop);
            assert_eq!(observed["baseline_calls"], ready["baseline_calls"]);
            assert_eq!(observed["calls_after_payload"], ready["baseline_calls"]);
            assert_eq!(observed["clipboard_unchanged"], true);
            assert_eq!(
                observed["synthetic_clipboard_read_write_control_after"],
                true
            );
            assert_eq!(attempt["desktop"], desktop);
            assert_eq!(attempt["create_desktop"], false, "{attempt}");
            assert_eq!(attempt["switch_desktop"], false, "{attempt}");
            assert_eq!(attempt["send_message"], false, "{attempt}");
            let error = u32::try_from(attempt["send_error"].as_u64().unwrap()).unwrap();
            assert!(
                matches!(error, ERROR_ACCESS_DENIED | ERROR_INVALID_WINDOW_HANDLE),
                "USER-message timeout/generic failure is not denial: {attempt}"
            );
            // The account ACL now denies access before the clipboard can be
            // opened; a read without that prerequisite is not a valid read test.
            assert_eq!(attempt["clipboard"]["opened"], false, "{attempt}");
            assert_eq!(
                attempt["clipboard"]["open_error"], ERROR_ACCESS_DENIED,
                "{attempt}"
            );
            assert_eq!(attempt["clipboard"]["read_handle_null"], true, "{attempt}");
            assert_eq!(attempt["clipboard"]["empty_succeeded"], false, "{attempt}");
            assert_eq!(
                attempt["clipboard"]["empty_error"], ERROR_ACCESS_DENIED,
                "{attempt}"
            );
            assert_eq!(current_desktop_name(), host_desktop);
        })
        .await;
    fs::write(directory.path().join("ui-exit-gate"), b"finish").unwrap();
    let output = finish(&run, &provider).await;
    pinned.assert_all_stopped_now();
    assert_eq!(output["exit_code"], 0, "{output}");
    engine.close().await.unwrap();
    fixture.sandbox.limits = original_limits;
    println!(
        "NATIVE_TREE_UI_BOUNDARIES_OK private_station_desktop_clipboard_read_clear_and_outside_job_window"
    );
}

async fn console_budget_checks(fixture: &mut Fixture) {
    let original_limits = fixture.sandbox.limits.clone();
    fixture.sandbox.limits.processes = 2;
    for mode in ["console-alloc", "console-child"] {
        let directory = fixture.case(mode);
        let (engine, run, provider) = launch(
            fixture,
            directory.path(),
            fixture.arguments(directory.path()),
            Mode::Run,
        )
        .await;
        wait_for(directory.path().join("console-ready")).await;
        let pinned = fixture
            .snapshot_with_cleanup(directory.path(), 2, &run, &engine)
            .await;
        provider.completion.pin(&pinned);
        let before = fixture
            .assert_with_cleanup(&run, &engine, || {
                console_helpers()
                    .into_iter()
                    .map(|(pid, _, _)| pid)
                    .collect::<BTreeSet<_>>()
            })
            .await;
        fs::write(directory.path().join("console-attempt-gate"), b"attempt").unwrap();
        wait_for(directory.path().join("console-attempt.json")).await;
        let after_pinned = fixture.assert_with_cleanup(&run, &engine, || {
            let attempt = read_json(directory.path().join("console-attempt.json"));
            let helpers = new_console_helpers(&before);
            let evidence = json!({"mode":mode,"attempt":attempt,"new_console_helpers":helpers,"account_sid":fixture.provision["user_sid"]});
            println!("NATIVE_TREE_CONSOLE_BUDGET_OBSERVATION {evidence}");
            write_json(directory.path().join("console-budget-observation.json"), evidence);
            for handle in &pinned.0 {
                assert_eq!(unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) }, WAIT_TIMEOUT, "An original member exited during the full-budget console attempt");
            }
            let after = fixture.snapshot(directory.path(), 2);
            for helper in &helpers {
                assert!(helper.get("query_error").is_none(), "Cannot establish new console-helper containment: {helper}");
                assert_eq!(helper["wait_result"], WAIT_OBJECT_0, "A new live console helper exceeded the full process budget, including helpers outside the owned Job: {helper}");
            }
            if mode == "console-child" {
                assert_eq!(attempt["spawn_denied"], true, "CREATE_NEW_CONSOLE must not start an extra child at the full cap: {attempt}");
            } else {
                assert!(attempt["allocated"].is_boolean(), "Missing AllocConsole result: {attempt}");
            }
            after
        }).await;
        fs::write(directory.path().join("console-exit-gate"), b"finish").unwrap();
        let output = finish(&run, &provider).await;
        pinned.assert_all_stopped_now();
        after_pinned.assert_all_stopped_now();
        assert_eq!(output["exit_code"], 0, "{output}");
        engine.close().await.unwrap();
        println!("NATIVE_TREE_CONSOLE_BUDGET_OK {mode}");
    }
    fixture.sandbox.limits = original_limits;
}

#[tokio::test]
#[ignore = "selected native diagnostic only; never substitutes for the complete required acceptance"]
async fn diagnose_native_process_tree_case() {
    assert_eq!(
        std::env::var(REQUIRED_ENV).as_deref(),
        Ok("1"),
        "Selected diagnostics require {REQUIRED_ENV}=1 and real provisioning"
    );
    let selected = std::env::var("WW_TREE_DIAGNOSTIC_CASE").expect(
        "Select boundaries, inbound, resources, output, cargo, console, ui, dns or network-token",
    );
    let mut fixture = Fixture::configured().await.unwrap();
    println!(
        "NATIVE_TREE_DIAGNOSTIC_ONLY_BEGIN {selected}; full required acceptance remains separate"
    );
    match selected.as_str() {
        "boundaries" => boundary_checks(&fixture).await,
        "inbound" => inbound_loopback_checks(&fixture).await,
        "resources" => resource_checks(&mut fixture).await,
        "output" => bounded_output_checks(&mut fixture).await,
        "cargo" => offline_cargo(&mut fixture).await,
        "console" => console_budget_checks(&mut fixture).await,
        "network-token" => thread_identity_network_checks(&mut fixture).await,
        "dns" => dns_broker_checks(&fixture).await,
        "ui" => ui_boundary_checks(&mut fixture).await,
        _ => {
            panic!("Unknown native diagnostic case {selected}; no full-suite cases may be skipped")
        }
    }
    println!("NATIVE_TREE_DIAGNOSTIC_ONLY_PASSED {selected}; this is not full acceptance");
}

async fn abrupt_host_death(fixture: &Fixture) {
    let directory = fixture.case("tree");
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
    let relative = next
        .path()
        .strip_prefix(&fixture.workspace.path)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    let source = format!("{relative}/source.txt");
    let mut arguments = fixture.arguments(next.path());
    arguments["verification"] = json!({"kind":"behavior","criteria":"guard before verification file reads","paths":[source]});
    // Prepare a valid authorization before the crashed invocation. Execute must
    // recheck the persistent barrier, before its verification-files read.
    let prepared_before_crash = tool.prepare(arguments.clone(), &context).await.unwrap();
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
    pinned.wait_after_abrupt_host_death();
    let marker = fixture
        .manifest
        .parent()
        .unwrap()
        .join("incomplete-run.json");
    assert!(
        marker.is_file(),
        "Abrupt host exit must retain the protected incomplete-run receipt"
    );
    let error = tool.prepare(arguments, &context).await.expect_err(
        "Process preparation must refuse unconfirmed cleanup before verification reads",
    );
    assert_eq!(error.code, "cleanup_required", "{error}");
    let error = tool.execute(prepared_before_crash, &context).await.expect_err(
        "Previously prepared process authorization must not bypass the persistent cleanup barrier");
    assert_eq!(error.code, "cleanup_required", "{error}");
    let blocked_write = format!("{relative}/after-crash-must-not-write.txt");
    for (name, args) in [
        ("files.read", json!({"path":source})),
        ("files.list", json!({"path":relative})),
        (
            "files.write",
            json!({"path":blocked_write,"content":"must not write after unconfirmed cleanup"}),
        ),
    ] {
        let file_tool = ToolRegistry::builtins().unwrap().get(name).unwrap();
        let preparation = file_tool
            .prepare(args.clone(), &context)
            .await
            .expect_err("File preparation bypassed the persistent workspace barrier");
        assert_eq!(
            preparation.code, "cleanup_required",
            "{name}: {preparation}"
        );
        let execution = file_tool
            .execute(args, &context)
            .await
            .expect_err("Direct file execution bypassed the persistent workspace barrier");
        assert_eq!(execution.code, "cleanup_required", "{name}: {execution}");
    }
    assert!(!next.path().join("after-crash-must-not-write.txt").exists());
    for name in ["process.status", "process.cancel"] {
        let control = ToolRegistry::builtins().unwrap().get(name).unwrap();
        let args = json!({"job_id":"unknown-after-crash"});
        let prepared = control
            .prepare(args, &context)
            .await
            .expect("Cleanup control preparation must remain available");
        if let Err(error) = control.execute(prepared, &context).await {
            assert_ne!(
                error.code, "cleanup_required",
                "Cleanup controls must bypass workspace admission"
            );
            assert_ne!(
                error.code, "sandbox_unavailable",
                "Cleanup controls must bypass profile startup"
            );
        }
    }
    println!(
        "NATIVE_TREE_CRASH_WORKSPACE_BARRIER_OK read_write_list_prepare_execute_and_verification"
    );
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
    let (engine, run, provider) = launch(
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
    let _pinned = fixture
        .snapshot_with_cleanup(&directory, 4, &run, &engine)
        .await;
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
    let (engine, run, provider) = launch(
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
    let pinned = fixture
        .snapshot_with_cleanup(directory.path(), 4, &run, &engine)
        .await;
    provider.completion.pin(&pinned);
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
    let output = finish(&run, &provider).await;
    assert_eq!(output["exit_code"], 0, "{output}");
    pinned.assert_all_stopped_now();
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

async fn fixture_file_with_cleanup(
    fixture: &Fixture,
    path: PathBuf,
    run: &RunHandle,
    engine: &WorkerEngine,
) {
    use futures_util::FutureExt;
    let observed = std::panic::AssertUnwindSafe(wait_for(path))
        .catch_unwind()
        .await;
    fixture
        .assert_with_cleanup(run, engine, || {
            if let Err(panic) = observed {
                std::panic::resume_unwind(panic);
            }
        })
        .await;
}
fn assert_network_identity_observation(
    host: bool,
    variant: &str,
    result: &Value,
    dns_arrivals: usize,
    socket_arrivals: [usize; 4],
    tcp_connections: [usize; 2],
) {
    println!(
        "NATIVE_TREE_THREAD_IDENTITY_NETWORK_OBSERVATION {}",
        json!({
            "host_control":host,"variant":variant,"result":result,
            "dns_arrivals":dns_arrivals,"socket_arrivals":socket_arrivals,"tcp_connections":tcp_connections,
            "socket_order":["tcp-v4","tcp-v6","udp-v4","udp-v6"],
        })
    );
    assert_eq!(result["variant"], variant);
    assert_eq!(
        result["winsock"]["startup_status"], 0,
        "Winsock initialization failed before identity change: {result}"
    );
    assert_eq!(result["winsock"]["requested_version"], 0x0202);
    assert_eq!(result["winsock"]["version"], 0x0202, "{result}");
    assert_eq!(
        result["winsock"]["cleanup"]["status"], 0,
        "Winsock cleanup after identity restoration failed: {result}"
    );
    if !host {
        assert_eq!(
            tcp_connections, [0; 2],
            "Server accepted a sandbox TCP connection despite reported API outcome: {result}"
        );
        assert_eq!(
            dns_arrivals, 0,
            "Thread identity bypass reached synthetic DNS: {result}"
        );
        assert_eq!(
            socket_arrivals, [0; 4],
            "Thread identity bypass reached a host network witness: {result}"
        );
    }
    if result["applied"] == false {
        assert_ne!(variant, "baseline");
        assert_eq!(result["network_attempted"], false);
        assert!(
            result["impersonation_error"]
                .as_u64()
                .is_some_and(|error| error != 0),
            "Missing explicit impersonation failure: {result}"
        );
        println!(
            "NATIVE_TREE_THREAD_IDENTITY_UNAVAILABLE host={host} variant={variant}; network calls were not attempted"
        );
        return;
    }
    assert_eq!(result["applied"], true, "{result}");
    assert_eq!(result["network_attempted"], true, "{result}");
    assert_eq!(
        result["revert"],
        if variant == "baseline" {
            "not-needed"
        } else {
            "succeeded"
        }
    );
    let operations = result["transport"].as_array().unwrap();
    assert_eq!(operations.len(), 4);
    if host {
        if variant == "baseline" || result["dns"]["status"] == 0 {
            assert_eq!(
                result["dns"]["status"], 0,
                "Host DNS baseline failed: {result}"
            );
            assert_eq!(result["dns"]["record"], true, "{result}");
            assert!(
                dns_arrivals > 0,
                "Host DNS API success without controlled arrival: {result}"
            );
        }
        for (operation, arrivals) in operations.iter().zip(socket_arrivals) {
            if variant == "baseline" {
                assert_eq!(operation["sent"], true, "Host baseline failed: {result}");
            }
            if operation["sent"] == true {
                assert!(
                    arrivals > 0,
                    "Host socket API success without controlled arrival: {result}"
                );
            }
        }
    } else {
        assert_ne!(
            result["dns"]["status"], 0,
            "Sandbox DNS unexpectedly succeeded: {result}"
        );
        assert_eq!(
            result["dns"]["record"], false,
            "Sandbox received a DNS record: {result}"
        );
        for operation in &operations[..2] {
            assert_eq!(
                operation["connected"], false,
                "Sandbox established TCP under thread impersonation: {result}"
            );
        }
        // UDP send success alone does not establish delivery. The independent
        // listeners above are authoritative even when an API reports failure.
    }
}
fn network_plan(variant: &str, dns: &tree_dns::Server, network: &tree_network::Witness) -> Value {
    let nonce = format!("ww-tree-net-{}", uuid::Uuid::new_v4());
    json!({"mode":"network-token","impersonation":variant,
        "dns_server":dns.address(),"dns_name":format!("{nonce}.ww-tree.test."),
        "network_nonce":nonce,"network":network.configuration()})
}
fn network_cell_connections(network: &tree_network::Witness, before: [usize; 2]) -> [usize; 2] {
    let after = network.connections();
    std::array::from_fn(|i| {
        // Exactly one subsequent host positive-control connection per family.
        assert!(after[i] > before[i], "Missing host TCP observer control");
        after[i] - before[i] - 1
    })
}
async fn host_thread_identity_network_cell(
    directory: &Path,
    plan: &Value,
    dns: &tree_dns::Server,
    network: &tree_network::Witness,
) {
    let before = network.connections();
    let mut control = Host(
        Command::new(directory.join("probe.exe"))
            .args(payload_args())
            .current_dir(directory)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for(directory.join("network-ready")).await;
    fs::write(directory.join("network-attempt-gate"), b"attempt").unwrap();
    wait_for(directory.join("network-result.json")).await;
    let result = read_json(directory.join("network-result.json"));
    // Distinct control nonces prove observer health after the child operation.
    dns.positive_control(&format!(
        "control-{}.ww-tree.test.",
        uuid::Uuid::new_v4().simple()
    ));
    network.positive_control();
    assert_network_identity_observation(
        true,
        plan["impersonation"].as_str().unwrap(),
        &result,
        dns.arrivals(plan["dns_name"].as_str().unwrap()),
        network.arrivals(plan["network_nonce"].as_str().unwrap()),
        network_cell_connections(network, before),
    );
    fs::write(directory.join("network-exit-gate"), b"finish").unwrap();
    assert!(
        control.0.wait().unwrap().success(),
        "Host network variant failed"
    );
}
#[tokio::test]
#[ignore = "normal-host thread identity controls only; never qualifies the sandbox profile"]
async fn diagnose_thread_identity_host_network() {
    let dns = tree_dns::Server::new();
    let network = tree_network::Witness::new();
    network.positive_control();
    for variant in ["baseline", "self-anonymous", "anonymous-token"] {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("probe.exe"),
            fs::read(std::env::current_exe().unwrap()).unwrap(),
        )
        .unwrap();
        let plan = network_plan(variant, &dns, &network);
        write_json(directory.path().join("plan.json"), plan.clone());
        host_thread_identity_network_cell(directory.path(), &plan, &dns, &network).await;
    }
    println!("NATIVE_TREE_HOST_ONLY_THREAD_IDENTITY_CONTROLS_OK; not sandbox acceptance");
}
async fn thread_identity_network_checks(fixture: &mut Fixture) {
    let original_limits = fixture.sandbox.limits.clone();
    fixture.sandbox.limits.processes = 2;
    fixture.sandbox.limits.timeout_ms = 30_000;
    let dns = tree_dns::Server::new();
    let network = tree_network::Witness::new();
    network.positive_control();
    for variant in ["baseline", "self-anonymous", "anonymous-token"] {
        for host in [true, false] {
            let directory = fixture.case("network-token");
            let plan = network_plan(variant, &dns, &network);
            write_json(directory.path().join("plan.json"), plan.clone());
            if host {
                host_thread_identity_network_cell(directory.path(), &plan, &dns, &network).await;
                continue;
            }
            let before = network.connections();
            let dns_name = plan["dns_name"].as_str().unwrap();
            let nonce = plan["network_nonce"].as_str().unwrap();
            let observed = directory.path().join("network-result.json");
            let (engine, run, provider) = launch(
                fixture,
                directory.path(),
                fixture.arguments(directory.path()),
                Mode::Run,
            )
            .await;
            fixture_file_with_cleanup(
                fixture,
                directory.path().join("network-ready"),
                &run,
                &engine,
            )
            .await;
            let pinned = fixture
                .snapshot_with_cleanup(directory.path(), 2, &run, &engine)
                .await;
            provider.completion.pin(&pinned);
            fs::write(directory.path().join("network-attempt-gate"), b"attempt").unwrap();
            fixture_file_with_cleanup(fixture, observed.clone(), &run, &engine).await;
            fixture
                .assert_with_cleanup(&run, &engine, || {
                    let result = read_json(&observed);
                    dns.positive_control(&format!(
                        "control-{}.ww-tree.test.",
                        uuid::Uuid::new_v4().simple()
                    ));
                    network.positive_control();
                    assert_network_identity_observation(
                        false,
                        variant,
                        &result,
                        dns.arrivals(dns_name),
                        network.arrivals(nonce),
                        network_cell_connections(&network, before),
                    );
                })
                .await;
            fs::write(directory.path().join("network-exit-gate"), b"finish").unwrap();
            let output = finish(&run, &provider).await;
            pinned.assert_all_stopped_now();
            assert_eq!(output["exit_code"], 0, "{output}");
            // Also reject any request arriving during final Job cleanup.
            fixture
                .assert_with_cleanup(&run, &engine, || {
                    assert_eq!(dns.arrivals(dns_name), 0);
                    assert_eq!(network.arrivals(nonce), [0; 4]);
                    assert_eq!(network_cell_connections(&network, before), [0; 2]);
                })
                .await;
            engine.close().await.unwrap();
        }
    }
    fixture.sandbox.limits = original_limits;
    println!("NATIVE_TREE_THREAD_IDENTITY_NETWORK_MATRIX_OK");
}

fn formatted_sid(sid: PSID) -> String {
    let mut text = std::ptr::null_mut();
    assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut text) }, 0);
    let mut length = 0;
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
    }
    let result = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    unsafe {
        LocalFree(text.cast());
    }
    result
}
fn current_token_group_sids(class: TOKEN_INFORMATION_CLASS) -> Vec<(String, u32)> {
    let mut token = std::ptr::null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0,
        "Query actual payload token: {}",
        std::io::Error::last_os_error()
    );
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut size = 0;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            class,
            std::ptr::null_mut(),
            0,
            &mut size,
        );
    }
    assert!(
        size > 0 && size <= 1 << 20,
        "Invalid token group size: {size}"
    );
    let mut storage = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                class,
                storage.as_mut_ptr().cast(),
                size,
                &mut size,
            )
        },
        0,
        "Read actual token groups: {}",
        std::io::Error::last_os_error()
    );
    let header = storage.as_ptr().cast::<TOKEN_GROUPS>();
    let count = unsafe { (*header).GroupCount } as usize;
    let offset = std::mem::offset_of!(TOKEN_GROUPS, Groups);
    assert!(
        offset + count * std::mem::size_of::<SID_AND_ATTRIBUTES>()
            <= storage.len() * std::mem::size_of::<usize>()
    );
    let groups = unsafe {
        std::slice::from_raw_parts(
            std::ptr::addr_of!((*header).Groups).cast::<SID_AND_ATTRIBUTES>(),
            count,
        )
    };
    groups
        .iter()
        .map(|group| (formatted_sid(group.Sid), group.Attributes))
        .collect()
}
fn host_logon_sid() -> String {
    let groups: Vec<_> = current_token_group_sids(TokenGroups)
        .into_iter()
        .filter(|(_, attributes)| {
            attributes & SE_GROUP_LOGON_ID as u32 == SE_GROUP_LOGON_ID as u32
                && attributes & SE_GROUP_ENABLED as u32 != 0
        })
        .collect();
    assert_eq!(
        groups.len(),
        1,
        "Require one enabled host logon SID: {groups:?}"
    );
    groups[0].0.clone()
}
struct TestSecurityDescriptor(PSECURITY_DESCRIPTOR);
impl Drop for TestSecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn host_logon_control_file(path: &Path, sid: &str) {
    use windows_sys::Win32::{
        Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
            SE_FILE_OBJECT,
        },
        Storage::FileSystem::*,
    };
    let mut descriptor = std::ptr::null_mut();
    assert_ne!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide(&format!("D:P(A;;FA;;;SY)(A;;FA;;;{sid})")).as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        },
        0
    );
    let descriptor = TestSecurityDescriptor(descriptor);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    {
        let handle = unsafe {
            CreateFileW(
                wide(&path.to_string_lossy()).as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            handle,
            INVALID_HANDLE_VALUE,
            "Create synthetic logon-SID file: {}",
            std::io::Error::last_os_error()
        );
        let mut creation = unsafe { fs::File::from_raw_handle(handle) };
        creation
            .write_all(b"synthetic host logon SID control")
            .unwrap();
    }
    // Reopen after closing the creation handle: creation itself is not a DACL
    // positive control. This file is inside the traversable test workspace.
    let reopened = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut actual_descriptor = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    assert_eq!(
        unsafe {
            GetSecurityInfo(
                reopened.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut actual_descriptor,
            )
        },
        ERROR_SUCCESS
    );
    let actual_descriptor = TestSecurityDescriptor(actual_descriptor);
    let mut control = 0;
    let mut revision = 0;
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(actual_descriptor.0, &mut control, &mut revision) },
        0
    );
    assert_ne!(control & SE_DACL_PROTECTED, 0);
    assert!(!dacl.is_null());
    assert_eq!(
        unsafe { (*dacl).AceCount },
        2,
        "Only SYSTEM and host logon SID may be granted access"
    );
    let mut trustees = BTreeSet::new();
    for index in 0..2 {
        let mut entry = std::ptr::null_mut();
        assert_ne!(unsafe { GetAce(dacl, index, &mut entry) }, 0);
        let ace = entry.cast::<ACCESS_ALLOWED_ACE>();
        assert_eq!(
            unsafe { (*ace).Header.AceType } as u32,
            ACCESS_ALLOWED_ACE_TYPE
        );
        assert_eq!(unsafe { (*ace).Header.AceFlags }, 0);
        assert_eq!(unsafe { (*ace).Mask }, FILE_ALL_ACCESS);
        trustees.insert(formatted_sid(unsafe {
            std::ptr::addr_of_mut!((*ace).SidStart).cast()
        }));
    }
    assert_eq!(
        trustees,
        BTreeSet::from(["S-1-5-18".to_string(), sid.to_string()])
    );
    drop(reopened);
    assert_eq!(fs::read(path).unwrap(), b"synthetic host logon SID control");
    fs::write(path, b"synthetic host logon SID control").unwrap();
    println!(
        "NATIVE_TREE_HOST_LOGON_ACL_CONTROL {}",
        json!({"sid":sid,"protected":true,"trustees":trustees,"fresh_read_write_handle":true})
    );
}
fn assert_payload_host_logon_boundary(plan: &Value, generation: usize) {
    let sid = plan["host_logon_sid"].as_str().unwrap();
    let enabled = current_token_group_sids(TokenGroups);
    let restricted = current_token_group_sids(TokenRestrictedSids);
    let matching_groups: Vec<_> = enabled.iter().filter(|(value, _)| value == sid).collect();
    let matching_restricted: Vec<_> = restricted
        .iter()
        .filter(|(value, _)| value == sid)
        .collect();
    let mut observation = json!({"pid":std::process::id(),"generation":generation,"host_logon_sid":sid,
        "group_entries":matching_groups,"restricting_entries":matching_restricted});
    println!("NATIVE_TREE_PAYLOAD_LOGON_SID {observation}");
    assert!(
        matching_groups
            .iter()
            .all(|(_, attributes)| attributes & SE_GROUP_ENABLED as u32 == 0),
        "Payload retains an enabled host logon SID: {observation}"
    );
    assert!(
        matching_restricted.is_empty(),
        "Payload retains host logon SID as restricting SID: {observation}"
    );
    let path = Path::new(plan["host_logon_file"].as_str().unwrap());
    let read = fs::read(path).unwrap_err();
    let write = fs::OpenOptions::new().write(true).open(path).unwrap_err();
    observation["read_error"] = json!(read.raw_os_error());
    observation["write_error"] = json!(write.raw_os_error());
    println!("NATIVE_TREE_PAYLOAD_LOGON_FILE_DENIAL {observation}");
    assert_eq!(
        read.raw_os_error(),
        Some(ERROR_ACCESS_DENIED as i32),
        "{observation}"
    );
    assert_eq!(
        write.raw_os_error(),
        Some(ERROR_ACCESS_DENIED as i32),
        "{observation}"
    );
    write_json(format!("logon-boundary-{generation}.json"), observation);
}

async fn boundary_checks(fixture: &Fixture) {
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
    let directory = fixture.case("boundaries");
    let host_logon_sid = host_logon_sid();
    let host_logon_file = directory.path().join("host-logon-only.txt");
    host_logon_control_file(&host_logon_file, &host_logon_sid);
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("host-only.txt");
    fs::write(&secret, b"synthetic host control").unwrap();
    assert_eq!(fs::read(&secret).unwrap(), b"synthetic host control");
    fs::write(directory.path().join("allowed.txt"), b"allowed workspace").unwrap();
    let toolchain_root = PathBuf::from(fixture.provision["toolchain_roots"][0].as_str().unwrap());
    let toolchain_control = tempfile::Builder::new()
        .prefix("ww-readonly-control-")
        .tempfile_in(&toolchain_root)
        .unwrap()
        .into_temp_path();
    fs::write(&toolchain_control, b"synthetic read-only toolchain input").unwrap();
    let private_temp = tempfile::Builder::new()
        .prefix("ww-private-temp-control-")
        .tempdir_in(fixture.provision["private_temp"].as_str().unwrap())
        .unwrap();
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
        json!({"mode":"boundaries","host_logon_sid":host_logon_sid,"host_logon_file":host_logon_file,"outside":secret,"toolchain_control":toolchain_control.to_path_buf(),"private_temp":private_temp.path(),"pipe":pipe_name,"registry":registry_path,"host_handle":host_file.as_raw_handle() as usize as u64,"host_volume":identity.VolumeSerialNumber,"host_file_id":identity.FileId.Identifier,"endpoints":endpoints.map(|e|e.to_string())}),
    );
    let (engine, run, provider) = launch(
        fixture,
        directory.path(),
        fixture.arguments(directory.path()),
        Mode::Run,
    )
    .await;
    for generation in 0..3 {
        fixture_file_with_cleanup(
            fixture,
            directory.path().join(format!("receipt-{generation}.json")),
            &run,
            &engine,
        )
        .await;
    }
    let pinned = fixture
        .snapshot_with_cleanup(directory.path(), 4, &run, &engine)
        .await;
    provider.completion.pin(&pinned);
    fs::write(directory.path().join("boundary-gate"), b"finish").unwrap();
    let output = finish(&run, &provider).await;
    assert_eq!(output["exit_code"], 0, "{output}");
    for generation in 0..3 {
        assert!(
            output["stdout"]
                .as_str()
                .unwrap()
                .contains(&format!("BOUNDARIES_OK_{generation}"))
        );
    }
    pinned.assert_all_stopped_now();
    engine.close().await.unwrap();
    // Denials are accepted only while the same targets remain host-accessible.
    assert_eq!(
        fs::read(&host_logon_file).unwrap(),
        b"synthetic host logon SID control"
    );
    fs::write(
        &host_logon_file,
        b"host logon SID write after payload denial",
    )
    .unwrap();
    assert_eq!(
        fs::read(&host_logon_file).unwrap(),
        b"host logon SID write after payload denial"
    );
    for generation in 0..3 {
        let evidence = read_json(
            directory
                .path()
                .join(format!("logon-boundary-{generation}.json")),
        );
        assert_eq!(evidence["host_logon_sid"], host_logon_sid);
        assert_eq!(
            evidence["pid"],
            read_json(directory.path().join(format!("receipt-{generation}.json")))["pid"]
        );
        assert_eq!(evidence["read_error"], ERROR_ACCESS_DENIED);
        assert_eq!(evidence["write_error"], ERROR_ACCESS_DENIED);
    }
    println!("NATIVE_TREE_HOST_LOGON_SID_BOUNDARY_OK generations=3");
    assert_eq!(fs::read(secret).unwrap(), b"synthetic host control");
    assert_eq!(
        fs::read(&toolchain_control).unwrap(),
        b"synthetic read-only toolchain input"
    );
    fs::write(
        &toolchain_control,
        b"host write control after sandbox denial",
    )
    .unwrap();
    for generation in 0..3 {
        assert_eq!(
            fs::read(
                private_temp
                    .path()
                    .join(format!("private-{generation}.txt"))
            )
            .unwrap(),
            b"private temp read/write"
        );
    }
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
    let (engine, run, provider) = launch(
        fixture,
        directory.path(),
        fixture.arguments(directory.path()),
        Mode::Run,
    )
    .await;
    wait_for(directory.path().join("listeners.json")).await;
    let pinned = fixture
        .snapshot_with_cleanup(directory.path(), 2, &run, &engine)
        .await;
    provider.completion.pin(&pinned);
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
    let output = finish(&run, &provider).await;
    assert_eq!(output["exit_code"], 0, "{output}");
    pinned.assert_all_stopped_now();
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
        let (engine, run, provider) = launch(
            fixture,
            directory.path(),
            fixture.arguments(directory.path()),
            Mode::Run,
        )
        .await;
        wait_for(directory.path().join("resource-ready")).await;
        let pinned = fixture
            .snapshot_with_cleanup(directory.path(), 2, &run, &engine)
            .await;
        provider.completion.pin(&pinned);
        fs::write(directory.path().join("resource-gate"), b"go").unwrap();
        let output = finish(&run, &provider).await;
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
        pinned.assert_all_stopped_now();
        engine.close().await.unwrap();
        println!("NATIVE_TREE_RESOURCE_OK {mode}");
    }
}

async fn bounded_output_checks(fixture: &mut Fixture) {
    let directory = fixture.case("output");
    fixture.sandbox.limits = ProcessLimits {
        processes: 4,
        timeout_ms: 20_000,
        memory_bytes: 512 << 20,
        cpu_seconds: 10,
        output_bytes: 4096,
        ..Default::default()
    };
    let (engine, run, provider) = launch(
        fixture,
        directory.path(),
        fixture.arguments(directory.path()),
        Mode::Run,
    )
    .await;
    wait_for(directory.path().join("output-ready")).await;
    let pinned = fixture
        .snapshot_with_cleanup(directory.path(), 2, &run, &engine)
        .await;
    provider.completion.pin(&pinned);
    fs::write(directory.path().join("output-gate"), b"write").unwrap();
    let output = finish(&run, &provider).await;
    pinned.assert_all_stopped_now();
    assert_eq!(output["exit_code"], 0, "{output}");
    assert_eq!(
        output["timed_out"], false,
        "Output drainage blocked the producer"
    );
    assert_eq!(output["truncated"], true);
    assert_eq!(output["stdout"].as_str().unwrap().len(), 4096);
    assert_eq!(output["stderr"].as_str().unwrap().len(), 4096);
    assert_eq!(
        fs::read(directory.path().join("output-complete")).unwrap(),
        b"both large streams written"
    );
    engine.close().await.unwrap();
    println!("NATIVE_TREE_BOUNDED_OUTPUT_AND_DRAIN_OK");
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
    fs::write(directory.path().join("Cargo.toml"), "[package]\nname = \"tree-offline-acceptance\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[lib]\ndoctest = false\n[workspace]\n").unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        r#"#[test]
fn native_behavior() {
    assert_eq!(2 + 3, 5);
    std::fs::write("cargo-ready", b"compiled test is running").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !std::path::Path::new("cargo-exit-gate").is_file() {
        assert!(std::time::Instant::now() < deadline, "Host did not observe Cargo test handles");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::fs::write("native-artifact.txt", b"built and tested by provisioned Rust/MSVC\n").unwrap();
    println!("OFFLINE_NATIVE_CARGO_OK");
}
"#,
    )
    .unwrap();
    fixture.sandbox.limits = ProcessLimits {
        timeout_ms: 120_000,
        cpu_seconds: 120,
        memory_bytes: 2 << 30,
        processes: 32,
        ..Default::default()
    };
    let arguments = json!({"argv":[cargo,"test","--offline","--","--nocapture"],"cwd":directory.path().strip_prefix(&fixture.workspace.path).unwrap().to_string_lossy()});
    let (engine, run, provider) = launch(fixture, directory.path(), arguments, Mode::Run).await;
    wait_for_within(
        directory.path().join("cargo-ready"),
        Duration::from_secs(90),
    )
    .await;
    // The trusted runner, Cargo and the just-compiled native test executable are
    // held at a gate so their original handles remain available at tool return.
    let pinned = fixture
        .snapshot_with_cleanup(directory.path(), 3, &run, &engine)
        .await;
    provider.completion.pin(&pinned);
    fs::write(directory.path().join("cargo-exit-gate"), b"finish").unwrap();
    let output = finish(&run, &provider).await;
    pinned.assert_all_stopped_now();
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
    if plan["mode"] == "network-token" {
        fs::write("network-ready", b"fresh process, before any DNS call").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(25);
        while !Path::new("network-attempt-gate").is_file() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let result = tree_network::attempt(&plan);
        // attempt restores the thread before touching workspace files again.
        write_json("network-result.json", result);
        while !Path::new("network-exit-gate").is_file() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        return;
    }
    if plan["mode"] == "ui" {
        let original = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
        write_json("ui-ready.json", json!({"desktop":current_desktop_name()}));
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !Path::new("ui-attempt-gate").is_file() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let request = read_json("ui-request.json");
        let clipboard = tree_ui::clipboard_attempt(
            u32::try_from(request["clipboard_format"].as_u64().unwrap()).unwrap(),
            request["desktop"].as_str().unwrap(),
        );
        let desktop = current_desktop_name();
        assert_eq!(request["desktop"], desktop);
        unsafe { SetLastError(0) };
        let created = unsafe {
            CreateDesktopW(
                wide(&format!("werk-tree-hostile-{}", uuid::Uuid::new_v4())).as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                DESKTOP_CREATEWINDOW | DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS,
                std::ptr::null(),
            )
        };
        let create_error = if created.is_null() {
            unsafe { GetLastError() }
        } else {
            0
        };
        if !created.is_null() {
            unsafe {
                SetThreadDesktop(original);
                CloseDesktop(created);
            }
        }
        unsafe { SetLastError(0) };
        let switched = unsafe { SwitchDesktop(original) };
        let switch_error = if switched == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        let mut response = 0;
        unsafe { SetLastError(0) };
        let sent = unsafe {
            SendMessageTimeoutW(
                request["window"].as_u64().unwrap() as usize as HWND,
                UI_CANARY_MESSAGE,
                request["nonce"].as_u64().unwrap() as usize,
                0,
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                2000,
                &mut response,
            )
        };
        let send_error = if sent == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        write_json(
            "ui-attempt.json",
            json!({
                "desktop":desktop,"create_desktop":!created.is_null(),"create_error":create_error,
                "switch_desktop":switched != 0,"switch_error":switch_error,
                "send_message":sent != 0,"send_error":send_error,"response":response,
                "clipboard":clipboard,
            }),
        );
        while !Path::new("ui-exit-gate").is_file() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        println!("UI_BOUNDARIES_CHECKED");
        return;
    }
    if plan["mode"] == "console-alloc" || plan["mode"] == "console-child" {
        println!("CONSOLE_BUDGET_PAYLOAD_READY");
        std::io::stdout().flush().unwrap();
        fs::write("console-ready", b"ready").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !Path::new("console-attempt-gate").is_file() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let allocated = if plan["mode"] == "console-alloc" {
            let result = unsafe { AllocConsole() };
            let error = (result == 0).then(|| unsafe { GetLastError() });
            // AllocConsole can replace standard handles. File evidence remains
            // observable, and a successful console stays alive until the host
            // has checked both the owned Job and new console-helper processes.
            write_json(
                "console-attempt.json",
                json!({"allocated":result != 0,"error":error}),
            );
            result != 0
        } else {
            // Suspension prevents unexpected successful creation from running
            // child code. Preserve the failure and terminate only this known
            // retained child; never relabel successful creation as denial.
            let child = Command::new(std::env::current_exe().unwrap())
                .args(payload_args())
                .creation_flags(CREATE_NEW_CONSOLE | CREATE_SUSPENDED)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            let observation = match child {
                Err(error) => json!({"spawn_denied":true,"error":error.raw_os_error()}),
                Ok(mut child) => {
                    let pid = child.id();
                    child.kill().unwrap();
                    let exit = child.wait().unwrap();
                    json!({"spawn_denied":false,"unexpected_pid":pid,"exit_code":exit.code()})
                }
            };
            write_json("console-attempt.json", observation);
            false
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !Path::new("console-exit-gate").is_file() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        if allocated {
            assert_ne!(unsafe { FreeConsole() }, 0);
        }
        return;
    }
    if plan["mode"] == "output" {
        fs::write("output-ready", b"started").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !Path::new("output-gate").is_file() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        // Each stream exceeds both the host capture ceiling and pipe buffer.
        std::io::stdout()
            .write_all(&vec![b'O'; 256 * 1024])
            .unwrap();
        std::io::stdout().flush().unwrap();
        std::io::stderr()
            .write_all(&vec![b'E'; 256 * 1024])
            .unwrap();
        std::io::stderr().flush().unwrap();
        fs::write("output-complete", b"both large streams written").unwrap();
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
        assert_payload_host_logon_boundary(&plan, generation);
        fs::write("created.txt", b"workspace write").unwrap();
        let private = Path::new(plan["private_temp"].as_str().unwrap())
            .join(format!("private-{generation}.txt"));
        fs::write(&private, b"private temp read/write").unwrap();
        assert_eq!(fs::read(&private).unwrap(), b"private temp read/write");
        let toolchain_control = Path::new(plan["toolchain_control"].as_str().unwrap());
        assert_eq!(
            fs::read(toolchain_control).unwrap(),
            b"synthetic read-only toolchain input"
        );
        assert_eq!(
            fs::OpenOptions::new()
                .write(true)
                .open(toolchain_control)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied,
            "Sandbox must not modify the explicitly read-only toolchain"
        );
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
    let parent_exits = plan["mode"] == "parent-exit" && generation == 0;
    let exit_gate = if parent_exits {
        "parent-exit-gate"
    } else {
        "exit-gate"
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !Path::new(exit_gate).is_file() {
        assert!(
            std::time::Instant::now() < deadline,
            "Host gate never opened"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    if parent_exits {
        let child = &child.as_ref().unwrap().0;
        let grandchild = read_json("receipt-2.json");
        let grandchild_pid = grandchild["pid"].as_u64().unwrap() as u32;
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                grandchild_pid,
            )
        };
        assert!(
            !handle.is_null(),
            "Parent must observe its still-gated grandchild"
        );
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        assert_eq!(
            birth(handle.as_raw_handle()),
            grandchild["created"].as_u64().unwrap()
        );
        let child_state = unsafe { WaitForSingleObject(child.as_raw_handle(), 0) };
        let grandchild_state = unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) };
        assert_eq!(child_state, WAIT_TIMEOUT);
        assert_eq!(grandchild_state, WAIT_TIMEOUT);
        write_json(
            "parent-exit-observation.json",
            json!({"child_pid":child.id(),"child_state":child_state,"grandchild_pid":grandchild_pid,"grandchild_state":grandchild_state}),
        );
        // Bypass the fixture's Host::drop and child waits deliberately. Only
        // trusted host Job cleanup may terminate these two live descendants.
        std::process::exit(0);
    }
    if let Some(child) = &mut child {
        assert!(child.0.wait().unwrap().success());
    }
}
