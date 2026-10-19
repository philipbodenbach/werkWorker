//! Narrow filesystem, process, artifact and HTTP capabilities.
use crate::{core::*, policy::relative_path};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Workspace {
    root: Arc<File>,
    pub path: PathBuf,
    pub identity: String,
}
impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("identity", &self.identity)
            .finish()
    }
}
#[cfg(target_os = "linux")]
fn open_beneath(root: &File, path: &str, flags: i32, mode: u32) -> Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    relative_path(path)?;
    let path =
        std::ffi::CString::new(path).map_err(|_| WorkerError::new("path_denied", "NUL in path"))?;
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    // RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV.
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC | libc::O_NONBLOCK) as u64,
        mode: mode as u64,
        resolve: 0x08 | 0x04 | 0x01,
    };
    // SAFETY: pointers refer to live NUL-terminated path and kernel ABI struct.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(WorkerError::new(
            "path_denied",
            "Workspace path missing, unsafe or inaccessible",
        ));
    }
    // SAFETY: successful syscall returns a newly owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

impl Workspace {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        tokio::task::spawn_blocking(move || {
            let path = std::fs::canonicalize(path)?;
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                // FILE_FLAG_BACKUP_SEMANTICS permits opening a directory handle.
                opts.custom_flags(0x02000000);
            }
            let root = opts.open(&path)?;
            let metadata = root.metadata()?;
            if !metadata.is_dir() {
                return Err(WorkerError::new(
                    "workspace_conflict",
                    "Workspace must be a directory",
                ));
            }
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt;
                format!("{}:{}", metadata.dev(), metadata.ino())
            };
            #[cfg(not(unix))]
            let identity = fingerprint(&path);
            Ok(Self {
                root: Arc::new(root),
                path,
                identity,
            })
        })
        .await
        .map_err(|_| WorkerError::new("io_error", "Workspace worker failed"))?
    }
    pub async fn revalidate(&self) -> Result<()> {
        let current = Self::open(&self.path).await?;
        if current.identity != self.identity {
            return Err(WorkerError::new(
                "workspace_conflict",
                "Workspace root identity changed",
            ));
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    pub async fn read(&self, path: &str, max: usize) -> Result<Vec<u8>> {
        let root = self.root.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let file = open_beneath(&root, &path, libc::O_RDONLY, 0)?;
            let m = file.metadata()?;
            if !m.is_file() || m.len() > max as u64 {
                return Err(WorkerError::new("file_limit", "Not a regular bounded file"));
            }
            let mut data = Vec::new();
            file.take(max as u64 + 1).read_to_end(&mut data)?;
            if data.len() > max {
                return Err(WorkerError::new("file_limit", "File grew beyond limit"));
            }
            Ok(data)
        })
        .await
        .map_err(|_| WorkerError::new("io_error", "File worker failed"))?
    }
    #[cfg(target_os = "linux")]
    pub async fn list(&self, path: &str, max: usize) -> Result<Vec<String>> {
        let root = self.root.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let dir = open_beneath(&root, &path, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            #[cfg(target_os = "linux")]
            {
                use std::os::fd::AsRawFd;
                let mut names = Vec::new();
                // /proc/self/fd identifies the pinned descriptor, not a model path.
                for entry in std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') || entry.file_type()?.is_symlink() {
                        continue;
                    }
                    names.push(name);
                    if names.len() > max {
                        return Err(WorkerError::new(
                            "listing_limit",
                            "Directory has too many entries; choose a narrower path",
                        ));
                    }
                }
                names.sort();
                Ok(names)
            }
            #[cfg(not(target_os = "linux"))]
            {
                drop(dir);
                Err(WorkerError::new(
                    "sandbox_unavailable",
                    "Filesystem backend requires Linux",
                ))
            }
        })
        .await
        .map_err(|_| WorkerError::new("io_error", "File worker failed"))?
    }
    /// Atomic replacement in a pinned parent directory. expected=None is create-only.
    #[cfg(target_os = "linux")]
    pub async fn write(&self, path: &str, bytes: Vec<u8>, expected: Option<String>) -> Result<()> {
        relative_path(path)?;
        let root = self.root.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let p = Path::new(&path);
            let parent = p
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| WorkerError::new("path_denied", "Invalid filename"))?;
            let dir = open_beneath(
                &root,
                &parent.to_string_lossy(),
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )?;
            match (&expected, open_beneath(&dir, name, libc::O_RDONLY, 0)) {
                (Some(expected), Ok(mut old)) => {
                    let meta = old.metadata()?;
                    if !meta.is_file() || meta.len() > 1_048_576 {
                        return Err(WorkerError::new(
                            "write_conflict",
                            "Patch base is not a bounded regular file",
                        ));
                    }
                    let mut content = Vec::new();
                    Read::by_ref(&mut old)
                        .take(1_048_577)
                        .read_to_end(&mut content)?;
                    if hash(content) != *expected {
                        return Err(WorkerError::new(
                            "write_conflict",
                            "File changed since patch preparation",
                        ));
                    }
                }
                (Some(_), Err(_)) | (None, Ok(_)) => {
                    return Err(WorkerError::new(
                        "write_conflict",
                        "Expected file state changed",
                    ));
                }
                (None, Err(_)) => {}
            }
            let temp = format!(".werk-{}.tmp", uuid::Uuid::new_v4());
            let mut file = open_beneath(
                &dir,
                &temp,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            #[cfg(target_os = "linux")]
            {
                use std::os::fd::AsRawFd;
                let from = std::ffi::CString::new(temp).unwrap();
                let to = std::ffi::CString::new(name).unwrap();
                let flags = if expected.is_none() {
                    libc::RENAME_NOREPLACE
                } else {
                    0
                };
                // SAFETY: parent descriptor and path CStrings stay alive during syscall.
                let rc = unsafe {
                    libc::renameat2(
                        dir.as_raw_fd(),
                        from.as_ptr(),
                        dir.as_raw_fd(),
                        to.as_ptr(),
                        flags,
                    )
                };
                if rc != 0 {
                    unsafe {
                        libc::unlinkat(dir.as_raw_fd(), from.as_ptr(), 0);
                    }
                    return Err(WorkerError::new(
                        "write_conflict",
                        "Atomic file replacement failed",
                    ));
                }
                dir.sync_all()?;
                Ok(())
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(WorkerError::new(
                    "sandbox_unavailable",
                    "Atomic filesystem backend requires Linux",
                ))
            }
        })
        .await
        .map_err(|_| WorkerError::new("io_error", "File worker failed"))?
    }
    #[cfg(not(target_os = "linux"))]
    pub async fn read(&self, _: &str, _: usize) -> Result<Vec<u8>> {
        Err(WorkerError::new(
            "sandbox_unavailable",
            "Secure filesystem tools require Linux openat2",
        ))
    }
    #[cfg(not(target_os = "linux"))]
    pub async fn list(&self, _: &str, _: usize) -> Result<Vec<String>> {
        Err(WorkerError::new(
            "sandbox_unavailable",
            "Secure filesystem tools require Linux openat2",
        ))
    }
    #[cfg(not(target_os = "linux"))]
    pub async fn write(&self, _: &str, _: Vec<u8>, _: Option<String>) -> Result<()> {
        Err(WorkerError::new(
            "sandbox_unavailable",
            "Secure filesystem tools require Linux openat2",
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessLimits {
    pub timeout_ms: u64,
    pub memory_bytes: u64,
    pub cpu_seconds: u64,
    pub processes: u64,
    pub output_bytes: usize,
}
impl Default for ProcessLimits {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            memory_bytes: 1_073_741_824,
            cpu_seconds: 30,
            processes: 128,
            output_bytes: 1_048_576,
        }
    }
}
#[derive(Debug, Clone)]
pub struct Sandbox {
    pub bubblewrap: PathBuf,
    pub limits: ProcessLimits,
}
impl Default for Sandbox {
    fn default() -> Self {
        Self {
            bubblewrap: PathBuf::from("/usr/bin/bwrap"),
            limits: ProcessLimits::default(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub cancelled: bool,
    pub timed_out: bool,
}
async fn drain(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut buf = [0; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let available = limit.saturating_sub(output.len());
        output.extend_from_slice(&buf[..n.min(available)]);
        truncated |= n > available;
    }
    Ok((output, truncated))
}
impl Sandbox {
    pub async fn doctor(&self) -> Result<Value> {
        let dir = std::env::temp_dir().join(id("werk_doctor"));
        tokio::fs::create_dir(&dir).await?;
        let workspace = Workspace::open(&dir).await?;
        let result = self
            .run(
                &workspace,
                false,
                &["/bin/true".into()],
                ".",
                CancellationToken::new(),
            )
            .await;
        let _ = tokio::fs::remove_dir(&dir).await;
        let output = result?;
        if output.exit_code != Some(0) {
            return Err(WorkerError::new(
                "sandbox_unavailable",
                "Bubblewrap namespace/mount setup failed",
            ));
        }
        Ok(
            json!({"backend":"bubblewrap","network":"isolated","host_home":false,"root_mount":false,"resource_limits":self.limits}),
        )
    }
    pub async fn run(
        &self,
        workspace: &Workspace,
        writable: bool,
        argv: &[String],
        cwd: &str,
        cancel: CancellationToken,
    ) -> Result<ProcessOutput> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (workspace, writable, argv, cwd, cancel);
            return Err(WorkerError::new(
                "sandbox_unavailable",
                "Only Linux bubblewrap is supported",
            ));
        }
        #[cfg(target_os = "linux")]
        {
            if cancel.is_cancelled() {
                return Err(WorkerError::new(
                    "cancelled",
                    "Process was cancelled before start",
                ));
            }
            if argv.is_empty()
                || argv.len() > 128
                || argv.iter().any(|a| a.len() > 8192 || a.contains('\0'))
            {
                return Err(WorkerError::new("invalid_argv", "Invalid program/argv"));
            }
            relative_path(cwd)?;
            workspace.revalidate().await?;
            let mut command = tokio::process::Command::new(&self.bubblewrap);
            command
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            command.args([
                "--unshare-all",
                "--die-with-parent",
                "--new-session",
                "--cap-drop",
                "ALL",
                "--clearenv",
                "--ro-bind",
                "/usr",
                "/usr",
                "--symlink",
                "usr/bin",
                "/bin",
                "--symlink",
                "usr/lib",
                "/lib",
                "--symlink",
                "usr/lib64",
                "/lib64",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--tmpfs",
                "/tmp",
                "--dir",
                "/home",
                "--dir",
                "/run",
            ]);
            use std::os::fd::AsRawFd;
            let workspace_fd = workspace.root.as_raw_fd();
            command
                .arg(if writable {
                    "--bind-fd"
                } else {
                    "--ro-bind-fd"
                })
                .arg(workspace_fd.to_string())
                .arg("/workspace");
            command.args([
                "--chdir",
                &format!("/workspace/{cwd}"),
                "--setenv",
                "PATH",
                "/usr/bin:/bin",
                "--setenv",
                "HOME",
                "/tmp",
                "--setenv",
                "LANG",
                "C.UTF-8",
                "--",
            ]);
            command.args(argv);
            let limits = self.limits.clone();
            // RLIMIT_NPROC is charged to the real host UID, including threads,
            // not to the PID namespace. Reserve a bounded allowance above the
            // current UID baseline. This is a UID-wide ceiling, not a cgroup.
            let process_ceiling = host_task_baseline().await?.saturating_add(limits.processes);
            // SAFETY: only async-signal-safe libc calls before exec; no allocation/locks.
            unsafe {
                command.pre_exec(move || {
                    // Do not inherit the embedding host's control sockets/pipes.
                    // Only the pinned workspace descriptor is passed to bwrap,
                    // which consumes it for mounting and closes it before exec.
                    if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::fcntl(workspace_fd, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    for (resource, value) in [
                        (libc::RLIMIT_AS, limits.memory_bytes),
                        (libc::RLIMIT_CPU, limits.cpu_seconds),
                        (libc::RLIMIT_NPROC, process_ceiling),
                        (libc::RLIMIT_FSIZE, 16 * 1024 * 1024),
                        (libc::RLIMIT_NOFILE, 128),
                        (libc::RLIMIT_CORE, 0),
                    ] {
                        let limit = libc::rlimit {
                            rlim_cur: value,
                            rlim_max: value,
                        };
                        if libc::setrlimit(resource, &limit) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().map_err(|_| {
                WorkerError::new(
                    "sandbox_unavailable",
                    "Cannot start bubblewrap; no host fallback",
                )
            })?;
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            let out_task = tokio::spawn(drain(stdout, self.limits.output_bytes));
            let err_task = tokio::spawn(drain(stderr, self.limits.output_bytes));
            let mut cancelled = false;
            let mut timed_out = false;
            let status = tokio::select! {
                s = child.wait() => s?,
                _ = cancel.cancelled() => { cancelled=true; child.kill().await?; child.wait().await? },
                _ = tokio::time::sleep(Duration::from_millis(self.limits.timeout_ms)) => { timed_out=true; child.kill().await?; child.wait().await? },
            };
            // PID namespace teardown kills all descendants, including detached groups.
            let (out, ot) = out_task
                .await
                .map_err(|_| WorkerError::new("process_io", "Output worker failed"))??;
            let (err, et) = err_task
                .await
                .map_err(|_| WorkerError::new("process_io", "Output worker failed"))??;
            Ok(ProcessOutput {
                exit_code: status.code(),
                stdout: String::from_utf8_lossy(&out).into(),
                stderr: String::from_utf8_lossy(&err).into(),
                truncated: ot || et,
                cancelled,
                timed_out,
            })
        }
    }
}

async fn host_task_baseline() -> Result<u64> {
    tokio::task::spawn_blocking(|| {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: getuid has no preconditions.
            let uid = unsafe { libc::getuid() };
            let mut count = 0u64;
            for entry in std::fs::read_dir("/proc")? {
                let Ok(entry) = entry else {
                    continue;
                };
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .bytes()
                    .all(|b| b.is_ascii_digit())
                {
                    continue;
                }
                if entry.metadata().is_ok_and(|m| m.uid() == uid)
                    && let Ok(tasks) = std::fs::read_dir(entry.path().join("task"))
                {
                    count = count.saturating_add(tasks.count() as u64);
                }
            }
            if count > 100_000 {
                return Err(WorkerError::new(
                    "resource_limit",
                    "Host UID process baseline exceeds supported bounds",
                ));
            }
            Ok(count.max(1))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(0)
        }
    })
    .await
    .map_err(|_| WorkerError::new("resource_limit", "Unable to count host tasks"))?
}

struct Job {
    run_id: String,
    cancel: CancellationToken,
    output: Arc<Mutex<Option<Result<ProcessOutput>>>>,
    task: Option<tokio::task::JoinHandle<()>>,
}
pub struct ProcessJob {
    pub sandbox: Sandbox,
    pub workspace: Workspace,
    pub writable: bool,
    pub argv: Vec<String>,
    pub cwd: String,
    pub lease: Option<Arc<crate::parallel::WorkspaceLease>>,
}
#[derive(Default)]
pub struct JobManager {
    jobs: Mutex<BTreeMap<String, Job>>,
}
impl JobManager {
    pub async fn start(
        &self,
        run_id: &str,
        sandbox: Sandbox,
        workspace: Workspace,
        writable: bool,
        argv: Vec<String>,
        cwd: String,
    ) -> Result<String> {
        self.start_managed(
            run_id,
            ProcessJob {
                sandbox,
                workspace,
                writable,
                argv,
                cwd,
                lease: None,
            },
        )
        .await
    }
    pub async fn start_managed(&self, run_id: &str, job: ProcessJob) -> Result<String> {
        let ProcessJob {
            sandbox,
            workspace,
            writable,
            argv,
            cwd,
            lease,
        } = job;
        let mut jobs = self.jobs.lock().await;
        if jobs.len() >= 16 {
            return Err(WorkerError::new("job_limit", "Too many managed jobs"));
        }
        let job_id = id("job");
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let output = Arc::new(Mutex::new(None));
        let sink = output.clone();
        let task = tokio::spawn(async move {
            let _lease = lease;
            let result = sandbox
                .run(&workspace, writable, &argv, &cwd, child_cancel)
                .await;
            *sink.lock().await = Some(result);
        });
        jobs.insert(
            job_id.clone(),
            Job {
                run_id: run_id.into(),
                cancel,
                output,
                task: Some(task),
            },
        );
        Ok(job_id)
    }
    pub async fn status(&self, run_id: &str, job_id: &str, cancel: bool) -> Result<Value> {
        let jobs = self.jobs.lock().await;
        let job = jobs
            .get(job_id)
            .filter(|j| j.run_id == run_id)
            .ok_or_else(|| WorkerError::new("unknown_job", "Job does not belong to this run"))?;
        if cancel {
            job.cancel.cancel();
        }
        let output = job.output.lock().await;
        Ok(match output.as_ref() {
            None => json!({"job_id":job_id,"status":"running"}),
            Some(Ok(out)) => json!({"job_id":job_id,"status":"finished","output":out}),
            Some(Err(e)) => json!({"job_id":job_id,"status":"failed","error":e.code}),
        })
    }
    pub async fn cleanup(&self, run_id: &str) -> Result<()> {
        let mut jobs = self.jobs.lock().await;
        let ids: Vec<_> = jobs
            .iter()
            .filter(|(_, j)| j.run_id == run_id)
            .map(|(id, _)| id.clone())
            .collect();
        let mut removed = Vec::new();
        for id in ids {
            if let Some(mut job) = jobs.remove(&id) {
                job.cancel.cancel();
                if let Some(task) = job.task.take() {
                    removed.push(task);
                }
            }
        }
        drop(jobs);
        for task in removed {
            task.await
                .map_err(|_| WorkerError::new("cleanup_failed", "Job cleanup failed"))?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct ArtifactStore {
    root: Workspace,
}
impl ArtifactStore {
    pub fn root_path(&self) -> &Path {
        &self.root.path
    }
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        tokio::fs::create_dir_all(path).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
        }
        Ok(Self {
            root: Workspace::open(path).await?,
        })
    }
    pub async fn put(&self, session: &str, run: &str, bytes: Vec<u8>) -> Result<ArtifactRef> {
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(WorkerError::new("artifact_limit", "Artifact exceeds 4 MiB"));
        }
        if self.root.list(".", 1024).await?.len() >= 1024 {
            return Err(WorkerError::new("artifact_limit", "Artifact store is full"));
        }
        let reference = ArtifactRef {
            id: id("artifact"),
            session_id: session.into(),
            run_id: run.into(),
            bytes: bytes.len(),
            sha256: hash(&bytes),
            expires_at_ms: now_ms() + 7 * 86_400_000,
        };
        self.root.write(&reference.id, bytes, None).await?;
        Ok(reference)
    }
    pub async fn read(
        &self,
        reference: &ArtifactRef,
        session: &str,
        offset: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        validate_id(&reference.id)?;
        if reference.session_id != session || reference.expires_at_ms <= now_ms() || length > 65_536
        {
            return Err(WorkerError::new(
                "artifact_denied",
                "Artifact scope/retention/size violation",
            ));
        }
        let data = self.root.read(&reference.id, 4 * 1024 * 1024).await?;
        if hash(&data) != reference.sha256 {
            return Err(WorkerError::new(
                "artifact_conflict",
                "Artifact content changed",
            ));
        }
        let start = offset.min(data.len());
        let end = start.saturating_add(length).min(data.len());
        Ok(data[start..end].to_vec())
    }
}

pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && (64..=127).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                || (a == 192 && b == 0))
        }
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(|v| public_ip(IpAddr::V4(v)))
            .unwrap_or_else(|| {
                let s = ip.segments();
                (s[0] & 0xe000) == 0x2000
                    && !(s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] < 0x0200))
                    && s[0] != 0x2002
            }),
    }
}
pub async fn bounded_body(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > max as u64) {
        return Err(WorkerError::new("body_limit", "HTTP body exceeds limit"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| WorkerError::transient("network_error"))?
    {
        if bytes.len() + chunk.len() > max {
            return Err(WorkerError::new("body_limit", "HTTP body exceeds limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
/// No redirects, no inherited proxy, validated and pinned DNS addresses.
pub async fn fetch(url: &str, method: &str, allow_private_origin: Option<&str>) -> Result<Value> {
    let url =
        reqwest::Url::parse(url).map_err(|_| WorkerError::new("invalid_url", "Invalid URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(WorkerError::new(
            "url_denied",
            "URL credentials, scheme or fragment denied",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| WorkerError::new("invalid_url", "Missing host"))?;
    let addresses: Vec<SocketAddr> = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, url.port_or_known_default().unwrap())),
    )
    .await
    .map_err(|_| WorkerError::new("timeout", "DNS timeout"))??
    .collect();
    let private_ok = allow_private_origin.is_some_and(|o| o == url.origin().ascii_serialization());
    if addresses.is_empty()
        || addresses.len() > 16
        || (!private_ok && addresses.iter().any(|a| !public_ip(a.ip())))
    {
        return Err(WorkerError::new(
            "network_denied",
            "Resolved address is outside the granted public network",
        ));
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, &addresses)
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| WorkerError::new("http_client", "Cannot build HTTP broker"))?;
    let req = match method {
        "GET" => client.get(url),
        "HEAD" => client.head(url),
        _ => {
            return Err(WorkerError::new(
                "method_denied",
                "Only GET and HEAD are supported",
            ));
        }
    };
    let response = req
        .send()
        .await
        .map_err(|_| WorkerError::new("network_error", "HTTP broker request failed"))?;
    if response.status().is_redirection() {
        return Err(WorkerError::new(
            "redirect_denied",
            "Redirects are not followed",
        ));
    }
    let status = response.status().as_u16();
    let bytes = bounded_body(response, 1_048_576).await?;
    Ok(json!({"status":status,"body":String::from_utf8_lossy(&bytes)}))
}
