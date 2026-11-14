//! Host-owned supervision of a separately provisioned, unprivileged Windows tree.
//! No provisioning, account repair, elevation or backend fallback occurs here.
use super::{
    LiveOutput, ProcessOutput, Sandbox, Workspace, windows_provision, windows_tree_launcher,
    windows_tree_runner,
};
use crate::{core::*, parallel::WorkspaceLease, policy::relative_path};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsStr,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::*,
    System::{JobObjects::*, Pipes::*, SystemInformation::GetSystemDirectoryW, Threading::*},
};

const MAX_REQUEST: usize = 65536;
const PIPE_BUFFER: u32 = 65536;
const CLEANUP_REPORT_AFTER: Duration = Duration::from_secs(10);

fn failure(operation: &str) -> WorkerError {
    WorkerError::new(
        "sandbox_unavailable",
        format!("{operation}: {}", std::io::Error::last_os_error()),
    )
}
fn wide(value: impl AsRef<OsStr>) -> Result<Vec<u16>> {
    let mut value: Vec<_> = value.as_ref().encode_wide().collect();
    if value.contains(&0) {
        return Err(WorkerError::new("invalid_argv", "NUL in Windows argument"));
    }
    value.push(0);
    Ok(value)
}
fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
}
unsafe fn own(handle: HANDLE) -> Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(failure("Create native handle"));
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}
fn system_directory() -> Result<PathBuf> {
    let mut data = vec![0u16; 32768];
    let n = unsafe { GetSystemDirectoryW(data.as_mut_ptr(), data.len() as u32) } as usize;
    if n == 0 || n >= data.len() {
        return Err(failure("GetSystemDirectoryW"));
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&data[..n])))
}
fn quote(value: &OsStr) -> Vec<u16> {
    let mut result = vec![34];
    let mut slashes = 0;
    for ch in value.encode_wide() {
        if ch == 92 {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            92,
            if ch == 34 { slashes * 2 + 1 } else { slashes },
        ));
        slashes = 0;
        result.push(ch);
    }
    result.extend(std::iter::repeat_n(92, slashes * 2));
    result.push(34);
    result
}
fn environment(values: &BTreeMap<String, String>) -> Result<Vec<u16>> {
    let mut result = Vec::new();
    for (key, value) in values {
        if key.contains('=') || key.is_empty() {
            return Err(WorkerError::new(
                "sandbox_unavailable",
                "Invalid environment key",
            ));
        }
        result.extend(wide(format!("{key}={value}"))?);
    }
    result.push(0);
    Ok(result)
}
fn validate(sandbox: &Sandbox, writable: bool) -> Result<()> {
    let limits = &sandbox.limits;
    if !writable {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "The provisioned coding account requires explicit workspace write authority; no read-only fallback",
        ));
    }
    if limits.processes < 2
        || limits.processes > 4096
        || limits.memory_bytes == 0
        || limits.memory_bytes > usize::MAX as u64
        || limits.cpu_seconds == 0
        || limits.cpu_seconds > i64::MAX as u64 / 10_000_000
        || limits.timeout_ms == 0
        || limits.output_bytes == 0
        || limits.output_bytes > 16 * 1024 * 1024
        || Instant::now()
            .checked_add(Duration::from_millis(limits.timeout_ms))
            .is_none()
    {
        return Err(WorkerError::new(
            "invalid_limits",
            "Windows tree requires finite representable limits, at least two Job slots (runner plus payload), and at most 16 MiB output per stream",
        ));
    }
    Ok(())
}
pub(super) fn description(
    sandbox: &Sandbox,
    workspace: &Workspace,
    writable: bool,
) -> Result<Value> {
    validate(sandbox, writable)?;
    let config = sandbox
        .native
        .windows_process_tree
        .as_ref()
        .ok_or_else(|| {
            WorkerError::new(
                "sandbox_unavailable",
                "Windows process-tree profile not selected",
            )
        })?;
    let p = windows_provision::load(&config.provisioning, &workspace.path)?;
    Ok(json!({
        "backend":"windows-account-job-tree", "version":1,
        "provisioning":config.provisioning, "provisioning_sha256":p.manifest_digest,
        "account_sid":p.manifest.user_sid, "workspace":p.manifest.workspace,
        "toolchains":p.manifest.toolchain_roots, "environment":p.manifest.toolchain_environment,
        "temporary":p.manifest.private_temp, "runner":p.manifest.runner,
        "network_enforcement":"persistent WFP SID blocks: connect, receive/accept and resource assignment, IPv4 and IPv6, including loopback",
        "ipc":"dedicated account and object ACLs; private authenticated stdio pipes; Win32k disabled",
        "inheritance":"atomic host-owned Job; stdio handle allowlist; no breakaway",
        "cleanup":"trusted supervisor retains workspace lease and invocation lock until root signaled and Job empty",
        "effective_process_limit":sandbox.limits.processes, "trusted_runner_processes":1,
        "resource_limits":sandbox.limits, "memory_scope":"aggregate Job commit", "cpu_scope":"aggregate Job user time",
        "limitations":["Dedicated per-workspace account; one active tree per provision", "Workspace-write mode only", "Existing Windows ACL grants to ordinary users remain readable; no inode-deny overlay", "Private temp persists until explicit host cleanup", "No per-file size or handle-count limit", "Native acceptance requires administrative provisioning"]
    }))
}

struct Attributes {
    _storage: Vec<usize>,
    pointer: LPPROC_THREAD_ATTRIBUTE_LIST,
}
impl Attributes {
    fn new() -> Result<Self> {
        let mut bytes = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut bytes);
        }
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        let pointer = storage.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(pointer, 2, 0, &mut bytes) } == 0 {
            return Err(failure("Initialize process attributes"));
        }
        Ok(Self {
            _storage: storage,
            pointer,
        })
    }
    fn set<T>(&mut self, kind: u32, value: &T) -> Result<()> {
        if unsafe {
            UpdateProcThreadAttribute(
                self.pointer,
                0,
                kind as usize,
                (value as *const T).cast(),
                size_of::<T>(),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(failure("Apply mandatory process creation attribute"));
        }
        Ok(())
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.pointer) };
    }
}

struct Pipe {
    handle: Option<OwnedHandle>,
    connected: bool,
}
impl Pipe {
    fn create(name: &str, reads: bool, host: &str, user: &str) -> Result<Self> {
        let descriptor_text = wide(format!("D:P(A;;GA;;;SY)(A;;GA;;;{host})(A;;GRGW;;;{user})"))?;
        let mut descriptor = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                descriptor_text.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(failure("Build private pipe ACL"));
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let name = wide(name)?;
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                (if reads {
                    PIPE_ACCESS_INBOUND
                } else {
                    PIPE_ACCESS_OUTBOUND
                }) | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                &sa,
            )
        };
        unsafe {
            LocalFree(descriptor);
        }
        Ok(Self {
            handle: Some(unsafe { own(handle)? }),
            connected: false,
        })
    }
    fn handle(&self) -> HANDLE {
        self.handle.as_ref().map(raw).unwrap_or(null_mut())
    }
    fn connect(&mut self, pid: u32) -> Result<bool> {
        if self.connected {
            return Ok(true);
        }
        if unsafe { ConnectNamedPipe(self.handle(), null_mut()) } != 0 {
            // PIPE_NOWAIT initially succeeds by making an instance available.
            // Only ERROR_PIPE_CONNECTED on a later call proves connection.
            return Ok(false);
        } else {
            match unsafe { GetLastError() } {
                ERROR_PIPE_LISTENING => return Ok(false),
                // A client may write a startup diagnostic and exit before this
                // poll. Its buffered data and peer PID remain queryable; require
                // the same retained-process authentication below before reading.
                ERROR_PIPE_CONNECTED | ERROR_NO_DATA => {}
                _ => return Err(failure("Connect private runner pipe")),
            }
        }
        let mut actual = 0;
        if unsafe { GetNamedPipeClientProcessId(self.handle(), &mut actual) } == 0 || actual != pid
        {
            return Err(WorkerError::new(
                "sandbox_unavailable",
                "Runner pipe peer is not the retained suspended process",
            ));
        }
        self.connected = true;
        Ok(true)
    }
    fn write(&mut self, data: &[u8]) -> Result<usize> {
        let mut sent = 0;
        if unsafe {
            WriteFile(
                self.handle(),
                data.as_ptr(),
                data.len().min(16384) as u32,
                &mut sent,
                null_mut(),
            )
        } == 0
        {
            return match unsafe { GetLastError() } {
                ERROR_NO_DATA | ERROR_BROKEN_PIPE => {
                    self.handle.take();
                    Ok(data.len())
                }
                ERROR_PIPE_BUSY => Ok(0),
                _ => Err(failure("Write bounded runner input")),
            };
        }
        Ok(sent as usize)
    }
    fn drain(
        &self,
        capture: &mut Capture,
        live: &Option<Arc<LiveOutput>>,
        stderr: bool,
    ) -> Result<()> {
        self.drain_chunks(capture, live, stderr, 8)
    }
    // Only after all owned Job writers have exited: the remaining pipe contents
    // are finite, even when the kernel buffered more than its advisory size.
    fn drain_until_empty(
        &self,
        capture: &mut Capture,
        live: &Option<Arc<LiveOutput>>,
        stderr: bool,
    ) -> Result<()> {
        self.drain_chunks(capture, live, stderr, usize::MAX)
    }
    fn drain_chunks(
        &self,
        capture: &mut Capture,
        live: &Option<Arc<LiveOutput>>,
        stderr: bool,
        chunks: usize,
    ) -> Result<()> {
        for _ in 0..chunks {
            let mut available = 0;
            if unsafe {
                PeekNamedPipe(
                    self.handle(),
                    null_mut(),
                    0,
                    null_mut(),
                    &mut available,
                    null_mut(),
                )
            } == 0
            {
                return match unsafe { GetLastError() } {
                    ERROR_BROKEN_PIPE | ERROR_NO_DATA => Ok(()),
                    _ => Err(failure("Inspect bounded runner output")),
                };
            }
            if available == 0 {
                return Ok(());
            }
            let mut bytes = [0u8; 8192];
            let mut read = 0;
            if unsafe {
                ReadFile(
                    self.handle(),
                    bytes.as_mut_ptr(),
                    available.min(bytes.len() as u32),
                    &mut read,
                    null_mut(),
                )
            } == 0
            {
                return Err(failure("Read runner output"));
            }
            capture.append(&bytes[..read as usize]);
            if let Some(live) = live {
                let mut log = if stderr {
                    live.stderr.blocking_lock()
                } else {
                    live.stdout.blocking_lock()
                };
                log.total = log.total.saturating_add(read as usize);
                log.bytes.extend_from_slice(&bytes[..read as usize]);
                let excess = log.bytes.len().saturating_sub(65536);
                if excess != 0 {
                    log.bytes.drain(..excess);
                }
            }
        }
        Ok(())
    }
}
struct Capture {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}
impl Capture {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            truncated: false,
        }
    }
    fn append(&mut self, bytes: &[u8]) {
        let keep = bytes.len().min(self.limit.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&bytes[..keep]);
        self.truncated |= keep < bytes.len();
    }
}

struct Tree {
    job: OwnedHandle,
    process: OwnedHandle,
    auxiliary: Vec<OwnedHandle>,
    completion: Arc<Mutex<Option<tokio::sync::oneshot::Sender<Result<ProcessOutput>>>>>,
    pid: u32,
    cleaned: AtomicBool,
    _lease: Option<Arc<WorkspaceLease>>,
    _provision: windows_provision::Provisioning,
}
impl Tree {
    fn active(&self) -> Result<u32> {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        if unsafe {
            QueryInformationJobObject(
                raw(&self.job),
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(WorkerError::new(
                "cleanup_failed",
                "Cannot confirm owned Job process count",
            ));
        }
        Ok(info.ActiveProcesses)
    }
    fn exited(&self) -> Result<Option<i32>> {
        match unsafe { WaitForSingleObject(raw(&self.process), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(raw(&self.process), &mut code) } == 0 {
                    return Err(failure("Get runner exit code"));
                }
                Ok(Some(code as i32))
            }
            _ => Err(failure("Wait retained runner handle")),
        }
    }
    fn terminate(&self) {
        unsafe {
            TerminateJobObject(raw(&self.job), 1);
        }
    }
    fn drained(&self) -> bool {
        self.active().is_ok_and(|n| n == 0)
            && unsafe { WaitForSingleObject(raw(&self.process), 0) } == WAIT_OBJECT_0
            && self
                .auxiliary
                .iter()
                .all(|process| unsafe { WaitForSingleObject(raw(process), 0) } == WAIT_OBJECT_0)
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        if self.cleaned.load(Ordering::Acquire) {
            return;
        }
        // Runs on the trusted supervisor thread, including unwind/startup error.
        // Unknown cleanup never releases the lease or cross-host invocation lock.
        self.terminate();
        unsafe {
            TerminateProcess(raw(&self.process), 1);
            for process in &self.auxiliary {
                TerminateProcess(raw(process), 1);
            }
        }
        let started = Instant::now();
        while !self.drained() {
            if started.elapsed() > CLEANUP_REPORT_AFTER
                && let Some(tx) = self.completion.lock().unwrap().take()
            {
                let _ = tx.send(Err(WorkerError::new("cleanup_failed", "Startup cleanup remains unconfirmed; trusted supervisor retains workspace lease and invocation lock")));
            }
            std::thread::sleep(Duration::from_millis(20));
            self.terminate();
        }
        // A failed receipt removal keeps the next invocation unavailable.
        let _ = self._provision.confirm_cleanup();
    }
}
static ACTIVE: OnceLock<Mutex<HashMap<String, Weak<Tree>>>> = OnceLock::new();
fn active_trees() -> &'static Mutex<HashMap<String, Weak<Tree>>> {
    ACTIVE.get_or_init(Mutex::default)
}

fn limits(job: HANDLE) -> Result<JOBOBJECT_EXTENDED_LIMIT_INFORMATION> {
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    if unsafe {
        QueryInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            null_mut(),
        )
    } == 0
    {
        return Err(failure("Read effective Job limits"));
    }
    Ok(info)
}
fn create_job(sandbox: &Sandbox) -> Result<OwnedHandle> {
    let job = unsafe { own(CreateJobObjectW(null(), null()))? };
    let mut requested: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    requested.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_JOB_TIME;
    requested.BasicLimitInformation.ActiveProcessLimit = sandbox.limits.processes as u32;
    requested.BasicLimitInformation.PerJobUserTimeLimit =
        (sandbox.limits.cpu_seconds * 10_000_000) as i64;
    requested.JobMemoryLimit = sandbox.limits.memory_bytes as usize;
    if unsafe {
        SetInformationJobObject(
            raw(&job),
            JobObjectExtendedLimitInformation,
            (&requested as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(failure("Set mandatory aggregate Job limits"));
    }
    let actual = limits(raw(&job))?;
    if actual.BasicLimitInformation.LimitFlags != requested.BasicLimitInformation.LimitFlags
        || actual.BasicLimitInformation.ActiveProcessLimit
            != requested.BasicLimitInformation.ActiveProcessLimit
        || actual.BasicLimitInformation.PerJobUserTimeLimit
            != requested.BasicLimitInformation.PerJobUserTimeLimit
        || actual.JobMemoryLimit != requested.JobMemoryLimit
    {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "Job limits differ from the approved finite policy",
        ));
    }
    Ok(job)
}
fn verify_account(process: HANDLE, expected: &str) -> Result<()> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(failure("Open suspended runner token"));
    }
    let token = unsafe { own(token)? };
    let mut n = 0;
    unsafe {
        GetTokenInformation(raw(&token), TokenUser, null_mut(), 0, &mut n);
    }
    let mut buffer = vec![0usize; (n as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            raw(&token),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            n,
            &mut n,
        )
    } == 0
    {
        return Err(failure("Read suspended runner identity"));
    }
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(failure("Format runner identity"));
    }
    let mut len = 0;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
    }
    let actual = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    unsafe {
        LocalFree(text.cast());
    }
    let mut elevated = 0u32;
    if unsafe {
        GetTokenInformation(
            raw(&token),
            TokenElevation,
            (&mut elevated as *mut u32).cast(),
            4,
            &mut n,
        )
    } == 0
    {
        return Err(failure("Read suspended runner elevation"));
    }
    if actual != expected || elevated != 0 {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "Runner identity/elevation differs from provisioned low-privilege account",
        ));
    }
    Ok(())
}

fn resolve_program(
    argv: &[String],
    workspace: &Workspace,
    p: &windows_provision::Provisioning,
) -> Result<Vec<String>> {
    if argv.is_empty()
        || argv.len() > 128
        || argv.iter().any(|a| a.len() > 8192 || a.contains('\0'))
    {
        return Err(WorkerError::new(
            "invalid_argv",
            "Invalid Windows process-tree argv",
        ));
    }
    let system = std::fs::canonicalize(system_directory()?)?;
    let mut executable = PathBuf::from(&argv[0]);
    if executable.extension().is_none() {
        executable.set_extension("exe");
    }
    let given = executable.as_path();
    let candidates = if given.is_absolute() {
        vec![given.to_path_buf()]
    } else if given.components().count() > 1 {
        vec![workspace.path.join(given)]
    } else {
        let mut paths = vec![workspace.path.join(given), system.join(given)];
        if let Some(path) = p.manifest.toolchain_environment.get("PATH") {
            paths.extend(std::env::split_paths(path).map(|d| d.join(given)));
        }
        paths
    };
    let program = candidates
        .into_iter()
        .find_map(|path| std::fs::canonicalize(path).ok())
        .ok_or_else(|| {
            WorkerError::new(
                "invalid_argv",
                "Executable absent from explicit workspace/toolchain/System32 search paths",
            )
        })?;
    let in_toolchain = p
        .manifest
        .toolchain_roots
        .iter()
        .any(|root| std::fs::canonicalize(root).is_ok_and(|root| program.starts_with(root)));
    if (!program.starts_with(&workspace.path) && !program.starts_with(system) && !in_toolchain)
        || !program
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    {
        return Err(WorkerError::new(
            "path_denied",
            "Executable outside provisioned runtime roots or not an .exe",
        ));
    }
    let mut argv = argv.to_vec();
    argv[0] = program.to_string_lossy().into_owned();
    Ok(argv)
}

#[allow(clippy::too_many_arguments)]
fn supervise(
    sandbox: Sandbox,
    workspace: Workspace,
    writable: bool,
    argv: Vec<String>,
    cwd: String,
    cancel: CancellationToken,
    live: Option<Arc<LiveOutput>>,
    input: Vec<u8>,
    lease: Option<Arc<WorkspaceLease>>,
    completion: Arc<Mutex<Option<tokio::sync::oneshot::Sender<Result<ProcessOutput>>>>>,
) -> Result<ProcessOutput> {
    validate(&sandbox, writable)?;
    relative_path(&cwd)?;
    let config = sandbox.native.windows_process_tree.as_ref().unwrap();
    let p = windows_provision::load(&config.provisioning, &workspace.path)?;
    p.try_lock_invocation()?;
    let argv = resolve_program(&argv, &workspace, &p)?;
    let current = std::fs::canonicalize(workspace.path.join(&cwd))?;
    if !current.starts_with(&workspace.path) {
        return Err(WorkerError::new(
            "path_denied",
            "Working directory escapes provisioned workspace",
        ));
    }
    let system = system_directory()?;
    let windows = system
        .parent()
        .ok_or_else(|| WorkerError::new("sandbox_unavailable", "Missing Windows directory"))?;
    let mut env = p.manifest.toolchain_environment.clone();
    for (key, value) in [
        ("SystemRoot", windows),
        ("WINDIR", windows),
        ("TEMP", p.manifest.private_temp.as_path()),
        ("TMP", p.manifest.private_temp.as_path()),
        ("USERPROFILE", p.manifest.private_temp.as_path()),
        ("HOME", p.manifest.private_temp.as_path()),
    ] {
        env.insert(key.into(), value.to_string_lossy().into_owned());
    }
    env.entry("PATH".into())
        .or_insert_with(|| system.to_string_lossy().into_owned());
    let request = serde_json::to_vec(&windows_tree_runner::Request {
        version: 1,
        host_sid: p.manifest.host_sid.clone(),
        argv,
        cwd: current,
        environment: env.clone(),
    })?;
    if request.len() > MAX_REQUEST || input.len() > 1024 * 1024 {
        return Err(WorkerError::new(
            "invalid_argv",
            "Runner request or stdin exceeds its bounded transport",
        ));
    }
    let prefix = format!("\\\\.\\pipe\\werk-tree-{}", uuid::Uuid::new_v4());
    let host = &p.manifest.host_sid;
    let user = &p.manifest.user_sid;
    let mut control = Pipe::create(&format!("{prefix}.control"), false, host, user)?;
    let mut stdin = Pipe::create(&format!("{prefix}.stdin"), false, host, user)?;
    let mut stdout = Pipe::create(&format!("{prefix}.stdout"), true, host, user)?;
    let mut stderr = Pipe::create(&format!("{prefix}.stderr"), true, host, user)?;
    let bootstrap_prefix = format!(r"\\.\pipe\werk-launch-{}", uuid::Uuid::new_v4());
    // Repeating the host SID grants no sandbox account access to bootstrap data.
    let mut launch_request =
        Pipe::create(&format!("{bootstrap_prefix}.request"), false, host, host)?;
    let mut launch_reply = Pipe::create(&format!("{bootstrap_prefix}.reply"), true, host, host)?;
    let mut launch_ack = Pipe::create(&format!("{bootstrap_prefix}.ack"), false, host, host)?;
    let launch_bytes = serde_json::to_vec(&windows_tree_launcher::Request {
        version: 1,
        provisioning: std::fs::canonicalize(&config.provisioning)?,
        manifest_digest: p.manifest_digest.clone(),
        workspace: workspace.path.clone(),
        pipe_prefix: prefix,
        environment: env,
    })?;
    if launch_bytes.len() > MAX_REQUEST {
        return Err(WorkerError::new(
            "invalid_argv",
            "Launcher request exceeds transport bound",
        ));
    }
    let job = create_job(&sandbox)?;
    let mut parent = null_mut();
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentProcess(),
            GetCurrentProcess(),
            &mut parent,
            SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            1,
            0,
        )
    } == 0
    {
        return Err(failure("Pin host identity for trusted launcher"));
    }
    let parent = unsafe { own(parent)? };
    let mut attributes = Attributes::new()?;
    let jobs = [raw(&job)];
    let inherited = [raw(&parent)];
    attributes.set(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
    attributes.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &inherited)?;
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attributes.pointer;
    let runner = wide(&p.manifest.runner)?;
    let mut command = quote(p.manifest.runner.as_os_str());
    for arg in [
        "--host-launcher".to_owned(),
        "--pipe-prefix".to_owned(),
        bootstrap_prefix,
        "--host-pid".to_owned(),
        unsafe { GetCurrentProcessId() }.to_string(),
        "--host-handle".to_owned(),
        (raw(&parent) as usize).to_string(),
    ] {
        command.push(32);
        command.extend(quote(OsStr::new(&arg)));
    }
    command.push(0);
    if command.len() > 32767 {
        return Err(WorkerError::new(
            "invalid_argv",
            "Launcher command exceeds Windows limit",
        ));
    }
    // Trusted bootstrap receives only a minimal local OS environment. Toolchain
    // paths and sandbox cwd do not affect loading code in the host account.
    let bootstrap_env = BTreeMap::from([
        (
            "SystemRoot".to_owned(),
            windows.to_string_lossy().into_owned(),
        ),
        ("WINDIR".to_owned(), windows.to_string_lossy().into_owned()),
        ("PATH".to_owned(), system.to_string_lossy().into_owned()),
    ]);
    let env_block = environment(&bootstrap_env)?;
    let working = wide(&system)?;
    if cancel.is_cancelled() {
        return Err(WorkerError::new(
            "cancelled",
            "Cancelled before trusted launcher",
        ));
    }
    let expected_user = user.clone();
    let started = Instant::now();
    p.begin_invocation()?;
    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    if unsafe {
        CreateProcessW(
            runner.as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_SUSPENDED
                | DETACHED_PROCESS
                | EXTENDED_STARTUPINFO_PRESENT
                | CREATE_UNICODE_ENVIRONMENT,
            env_block.as_ptr().cast(),
            working.as_ptr(),
            &startup.StartupInfo,
            &mut info,
        )
    } == 0
    {
        let error = failure("Create trusted launcher with atomic Job assignment");
        p.confirm_cleanup()?; // No process was created; the owned Job is empty.
        return Err(error);
    }
    // Successful CreateProcessW returns both owned handles. Establish the lease
    // owner immediately, before any fallible verification or pipe operation.
    let mut tree = Tree {
        job,
        process: unsafe { OwnedHandle::from_raw_handle(info.hProcess) },
        auxiliary: Vec::new(),
        completion: completion.clone(),
        pid: info.dwProcessId,
        cleaned: AtomicBool::new(false),
        _lease: lease,
        _provision: p,
    };
    let launcher_thread = unsafe { own(info.hThread)? };
    let mut member = 0;
    if unsafe { IsProcessInJob(raw(&tree.process), raw(&tree.job), &mut member) } == 0
        || member == 0
    {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "Trusted launcher is outside mandatory owned Job",
        ));
    }
    if unsafe { ResumeThread(raw(&launcher_thread)) } != 1 {
        return Err(failure("Resume verified trusted launcher"));
    }
    let mut offset = 0;
    let mut reply = Capture::new(8192);
    let ready = loop {
        if cancel.is_cancelled() {
            return Err(WorkerError::new(
                "cancelled",
                "Cancelled during trusted launcher handoff",
            ));
        }
        if started.elapsed() >= Duration::from_millis(sandbox.limits.timeout_ms) {
            return Err(WorkerError::new(
                "sandbox_unavailable",
                "Trusted launcher handoff exceeded invocation timeout",
            ));
        }
        let reply_connected = launch_reply.connect(tree.pid)?;
        if reply_connected {
            launch_reply.drain(&mut reply, &None, false)?;
        }
        if reply.truncated {
            return Err(WorkerError::new(
                "sandbox_unavailable",
                "Oversized trusted launcher reply",
            ));
        }
        if let Some(end) = reply.bytes.iter().position(|byte| *byte == b'\n') {
            break serde_json::from_slice::<windows_tree_launcher::Reply>(&reply.bytes[..end])?;
        }
        if let Some(code) = tree.exited()? {
            return Err(WorkerError::new(
                "sandbox_unavailable",
                format!("Trusted launcher exited before handoff: {code}"),
            ));
        }
        let connected =
            reply_connected & launch_request.connect(tree.pid)? & launch_ack.connect(tree.pid)?;
        if connected && launch_request.handle.is_some() {
            offset += launch_request.write(&launch_bytes[offset..])?;
            if launch_request.handle.is_none() {
                return Err(WorkerError::new(
                    "process_io",
                    "Launcher closed request before delivery",
                ));
            }
            if offset == launch_bytes.len() {
                launch_request.handle.take();
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let (pid, source_process, source_thread) = match ready {
        windows_tree_launcher::Reply::Ready {
            version: 1,
            pid,
            process_handle,
            thread_handle,
        } => (pid, process_handle, thread_handle),
        windows_tree_launcher::Reply::Failed {
            version: 1,
            message,
        } => {
            return Err(WorkerError::new(
                "sandbox_unavailable",
                format!("Trusted launcher: {message}"),
            ));
        }
        _ => {
            return Err(WorkerError::new(
                "sandbox_unavailable",
                "Unsupported trusted launcher reply",
            ));
        }
    };
    let launcher_process = raw(&tree.process);
    let pull = |source: u64| -> Result<OwnedHandle> {
        let source = usize::try_from(source)
            .ok()
            .filter(|h| *h != 0 && *h != usize::MAX)
            .ok_or_else(|| {
                WorkerError::new("sandbox_unavailable", "Invalid launcher handoff handle")
            })?;
        let mut handle = null_mut();
        if unsafe {
            DuplicateHandle(
                launcher_process,
                source as HANDLE,
                GetCurrentProcess(),
                &mut handle,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(failure("Pull retained suspended runner handle"));
        }
        unsafe { own(handle) }
    };
    let process = pull(source_process)?;
    let launcher = std::mem::replace(&mut tree.process, process);
    tree.auxiliary.push(launcher);
    tree.pid = pid;
    // From the first successful pull, even a thread-copy failure is covered by
    // the native owner's retained-process fallback and lease lifetime.
    let thread = pull(source_thread)?;
    if unsafe { GetProcessId(raw(&tree.process)) } != pid
        || unsafe { GetProcessIdOfThread(raw(&thread)) } != pid
    {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "Runner handoff process/thread identity mismatch",
        ));
    }
    member = 0;
    if unsafe { IsProcessInJob(raw(&tree.process), raw(&tree.job), &mut member) } == 0
        || member == 0
    {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "Suspended account runner did not inherit the exact owned Job",
        ));
    }
    verify_account(raw(&tree.process), &expected_user)?;
    if unsafe { SuspendThread(raw(&thread)) } != 1 || unsafe { ResumeThread(raw(&thread)) } != 2 {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "Runner was not initially suspended during handoff",
        ));
    }
    if launch_ack.write(&[1])? != 1 || launch_ack.handle.is_none() {
        return Err(WorkerError::new(
            "process_io",
            "Cannot acknowledge verified runner handoff",
        ));
    }
    launch_ack.handle.take();
    // Release the transient trusted-launcher slot before any payload can start.
    loop {
        match unsafe { WaitForSingleObject(raw(&tree.auxiliary[0]), 0) } {
            WAIT_OBJECT_0 => break,
            WAIT_TIMEOUT => {}
            _ => return Err(failure("Wait retained trusted launcher")),
        }
        if cancel.is_cancelled()
            || started.elapsed() >= Duration::from_millis(sandbox.limits.timeout_ms)
        {
            return Err(WorkerError::new(
                "sandbox_unavailable",
                "Interrupted while waiting for trusted launcher cleanup",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut launcher_exit = 0;
    if unsafe { GetExitCodeProcess(raw(&tree.auxiliary[0]), &mut launcher_exit) } == 0
        || launcher_exit != 0
    {
        return Err(failure("Trusted launcher did not finish verified handoff"));
    }
    let tree = Arc::new(tree);
    active_trees()
        .lock()
        .unwrap()
        .insert(workspace.identity.clone(), Arc::downgrade(&tree));
    if unsafe { ResumeThread(raw(&thread)) } != 1 {
        return Err(failure("Resume verified account runner"));
    }
    let mut timed_out = false;
    let mut cancelled = false;
    let mut request_offset = 0;
    let mut input_offset = 0;
    let mut out = Capture::new(sandbox.limits.output_bytes);
    let mut err = Capture::new(sandbox.limits.output_bytes);
    let result = (|| -> Result<i32> {
        loop {
            if cancel.is_cancelled() {
                cancelled = true;
                return Ok(1);
            }
            if started.elapsed() >= Duration::from_millis(sandbox.limits.timeout_ms) {
                timed_out = true;
                return Ok(1);
            }
            // The runner opens stderr first so startup errors can be observed
            // even if it fails before opening the remaining channels.
            let error_connected = stderr.connect(tree.pid)?;
            if error_connected {
                stderr.drain(&mut err, &live, true)?;
            }
            if let Some(code) = tree.exited()? {
                return Ok(code);
            }
            let output_connected = stdout.connect(tree.pid)?;
            if output_connected {
                stdout.drain(&mut out, &live, false)?;
            }
            let connected = control.connect(tree.pid)?
                & stdin.connect(tree.pid)?
                & output_connected
                & error_connected;
            if connected {
                if control.handle.is_some() {
                    request_offset += control.write(&request[request_offset..])?;
                    if control.handle.is_none() {
                        return Err(WorkerError::new(
                            "process_io",
                            "Runner closed its control pipe before request delivery",
                        ));
                    }
                    if request_offset == request.len() {
                        control.handle.take();
                    }
                }
                if stdin.handle.is_some() {
                    if input_offset < input.len() {
                        input_offset += stdin.write(&input[input_offset..])?;
                    }
                    if input_offset == input.len() {
                        stdin.handle.take();
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    tree.terminate();
    let cleanup_started = Instant::now();
    while !tree.drained() {
        if cleanup_started.elapsed() > CLEANUP_REPORT_AFTER
            && let Some(tx) = completion.lock().unwrap().take()
        {
            let _=tx.send(Err(WorkerError::new("cleanup_failed","Job drainage remains unconfirmed; supervisor retains workspace lease and invocation lock")));
        }
        tree.terminate();
        std::thread::sleep(Duration::from_millis(20));
    }
    tree._provision.confirm_cleanup()?;
    tree.cleaned.store(true, Ordering::Release);
    active_trees().lock().unwrap().remove(&workspace.identity);
    if stdout.connected {
        stdout.drain_until_empty(&mut out, &live, false)?;
    }
    if stderr.connected {
        stderr.drain_until_empty(&mut err, &live, true)?;
    }
    let output = result.map(|exit_code| ProcessOutput {
        exit_code: Some(exit_code),
        stdout: String::from_utf8_lossy(&out.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&err.bytes).into_owned(),
        truncated: out.truncated || err.truncated,
        timed_out,
        cancelled,
    });
    // Release native owner and lease before the result promises completion.
    drop(tree);
    if let Some(tx) = completion.lock().unwrap().take() {
        let _ = tx.send(output.clone());
    }
    output
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    sandbox: &Sandbox,
    workspace: &Workspace,
    writable: bool,
    argv: &[String],
    cwd: &str,
    cancel: CancellationToken,
    live: Option<Arc<LiveOutput>>,
    stdin: Option<&str>,
    lease: Option<Arc<WorkspaceLease>>,
) -> Result<ProcessOutput> {
    workspace.revalidate().await?;
    if cwd != "." {
        workspace.list(cwd, 20_000).await?;
    }
    let guard = CancelOnDrop(cancel.child_token());
    let worker_cancel = guard.0.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let sandbox = sandbox.clone();
    let workspace = workspace.clone();
    let argv = argv.to_vec();
    let cwd = cwd.to_owned();
    let input = stdin.unwrap_or_default().as_bytes().to_vec();
    // The OS thread outlives Tokio tasks/runtimes and owns all cleanup resources.
    let completion = Arc::new(Mutex::new(Some(tx)));
    std::thread::Builder::new()
        .name("werk-windows-tree".into())
        .spawn(move || {
            let result = supervise(
                sandbox,
                workspace,
                writable,
                argv,
                cwd,
                worker_cancel,
                live,
                input,
                lease,
                completion.clone(),
            );
            if let Some(tx) = completion.lock().unwrap().take() {
                let _ = tx.send(result);
            }
        })
        .map_err(|e| {
            WorkerError::new(
                "sandbox_unavailable",
                format!("Start trusted Windows supervisor: {e}"),
            )
        })?;
    let result = rx.await.map_err(|_| {
        WorkerError::new(
            "cleanup_failed",
            "Trusted Windows supervisor failed; completion not confirmed",
        )
    })?;
    drop(guard);
    result
}

impl Sandbox {
    /// Host-only observation of this process's owned Job; never exposed as a tool.
    #[doc(hidden)]
    pub fn windows_process_tree_snapshot(&self, workspace: &Workspace) -> Result<Value> {
        let tree = active_trees()
            .lock()
            .unwrap()
            .get(&workspace.identity)
            .and_then(Weak::upgrade)
            .ok_or_else(|| {
                WorkerError::new(
                    "unknown_job",
                    "No active local process-tree Job for this workspace",
                )
            })?;
        let info = limits(raw(&tree.job))?;
        let mut storage =
            vec![0usize; 2 + info.BasicLimitInformation.ActiveProcessLimit as usize + 32];
        let list = storage
            .as_mut_ptr()
            .cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
        if unsafe {
            QueryInformationJobObject(
                raw(&tree.job),
                JobObjectBasicProcessIdList,
                list.cast(),
                (storage.len() * size_of::<usize>()) as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(failure("Snapshot exact owned Job members"));
        }
        let count = unsafe { (*list).NumberOfProcessIdsInList } as usize;
        if unsafe { (*list).NumberOfAssignedProcesses } as usize != count {
            return Err(WorkerError::new(
                "sandbox_unavailable",
                "Incomplete Job membership observation",
            ));
        }
        let ids = unsafe { std::slice::from_raw_parts((*list).ProcessIdList.as_ptr(), count) };
        let mut processes = Vec::new();
        let mut pinned = Vec::new();
        for id in ids {
            let handle = unsafe {
                own(OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                    0,
                    *id as u32,
                ))?
            };
            let mut member = 0;
            if unsafe { IsProcessInJob(raw(&handle), raw(&tree.job), &mut member) } == 0
                || member == 0
            {
                return Err(failure("Pin exact Job member"));
            }
            processes.push(*id as u32);
            pinned.push(handle);
        }
        Ok(
            json!({"active_processes":tree.active()?,"process_ids":processes,"membership_verified":true,"runner_pid":tree.pid,
            "limits":{"processes":info.BasicLimitInformation.ActiveProcessLimit,"memory_bytes":info.JobMemoryLimit,"cpu_seconds":info.BasicLimitInformation.PerJobUserTimeLimit/10_000_000,
            "kill_on_close":info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0,"breakaway":info.BasicLimitInformation.LimitFlags & (JOB_OBJECT_LIMIT_BREAKAWAY_OK|JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK)!=0}}),
        )
    }
}

pub(super) async fn doctor(sandbox: &Sandbox) -> Result<Value> {
    let config = sandbox.native.windows_process_tree.as_ref().unwrap();
    let metadata = std::fs::metadata(&config.provisioning)?;
    if metadata.len() > 65536 {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "Provisioning manifest exceeds limit",
        ));
    }
    let manifest: Value = serde_json::from_slice(&std::fs::read(&config.provisioning)?)?;
    let path = manifest["workspace"]
        .as_str()
        .ok_or_else(|| WorkerError::new("sandbox_unavailable", "Missing provisioned workspace"))?;
    let workspace = Workspace::open(path).await?;
    let descriptor = description(sandbox, &workspace, true)?;
    let argv = vec![
        system_directory()?
            .join("cmd.exe")
            .to_string_lossy()
            .into_owned(),
        "/d".into(),
        "/c".into(),
        "exit".into(),
        "0".into(),
    ];
    let output = run(
        sandbox,
        &workspace,
        true,
        &argv,
        ".",
        CancellationToken::new(),
        None,
        None,
        None,
    )
    .await?;
    if output.exit_code != Some(0) || output.cancelled || output.timed_out {
        return Err(WorkerError::new(
            "sandbox_probe_failed",
            "Provisioned Windows runner startup/exit probe failed",
        ));
    }
    Ok(descriptor)
}
