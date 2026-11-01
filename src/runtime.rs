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
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
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
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            return Err(WorkerError::new(
                "not_found",
                "Workspace file or parent directory does not exist",
            ));
        }
        return Err(WorkerError::new(
            "path_denied",
            "Workspace path is unsafe or inaccessible",
        ));
    }
    // SAFETY: successful syscall returns a newly owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

impl Workspace {
    /// Bounded discovery through pinned descriptors; never follow symlinks.
    pub(crate) async fn discover(
        &self,
        path: &str,
        recursive: bool,
        hidden: bool,
    ) -> Result<(Vec<String>, bool)> {
        #[cfg(target_os = "linux")]
        {
            let root = self.root.clone();
            let path = path.to_owned();
            tokio::task::spawn_blocking(move || {
                use std::os::fd::AsRawFd;
                let first = open_beneath(&root, &path, libc::O_RDONLY, 0)?;
                if first.metadata()?.is_file() {
                    return Ok((vec![path], false));
                }
                if !first.metadata()?.is_dir() {
                    return Err(WorkerError::new(
                        "path_denied",
                        "Expected a regular file or directory",
                    ));
                }
                let mut pending = vec![(path, 0)];
                let mut files = Vec::new();
                let mut scanned = 0;
                let mut incomplete = false;
                while let Some((dir, depth)) = pending.pop() {
                    let fd = open_beneath(&root, &dir, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
                    let mut entries = Vec::new();
                    for entry in std::fs::read_dir(format!("/proc/self/fd/{}", fd.as_raw_fd()))? {
                        scanned += 1;
                        if scanned > 20_000 {
                            incomplete = true;
                            break;
                        }
                        let entry = entry?;
                        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                            incomplete = true;
                            continue;
                        };
                        let kind = entry.file_type()?;
                        if kind.is_symlink()
                            || matches!(
                                name.as_str(),
                                ".git" | ".hg" | ".svn" | "node_modules" | "target" | "__pycache__"
                            )
                            || (!hidden && name.starts_with('.'))
                        {
                            continue;
                        }
                        entries.push((name, kind));
                    }
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    for (name, kind) in entries {
                        let child = if dir == "." {
                            name
                        } else {
                            format!("{dir}/{name}")
                        };
                        if kind.is_file() {
                            files.push(child);
                        } else if recursive && kind.is_dir() {
                            if depth < 32 {
                                pending.push((child, depth + 1));
                            } else {
                                incomplete = true;
                            }
                        }
                    }
                    if scanned > 20_000 {
                        break;
                    }
                }
                files.sort();
                Ok((files, incomplete))
            })
            .await
            .map_err(|_| WorkerError::new("io_error", "Discovery worker failed"))?
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (path, recursive, hidden);
            Err(WorkerError::new(
                "sandbox_unavailable",
                "Secure discovery requires Linux openat2",
            ))
        }
    }
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
                (Some(_), Err(_)) => {
                    return Err(WorkerError::new(
                        "write_conflict",
                        "Expected file is missing or inaccessible",
                    ));
                }
                (None, Ok(_)) => {
                    return Err(WorkerError::new(
                        "write_conflict",
                        "Target already exists. Preserve it and use files.edit with the latest observed expected_hash and a unique nonempty old anchor. Use files.write for replacement only if its advertised schema allows expected_hash.",
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
    /// Chromium uses a larger bounded thread allowance than command tools.
    pub browser_processes: u64,
    /// Persistent browser sessions retain multiple tabs and browser services.
    pub browser_session_memory_bytes: u64,
    pub browser_session_cpu_seconds: u64,
    pub output_bytes: usize,
}
impl Default for ProcessLimits {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            memory_bytes: 1_073_741_824,
            cpu_seconds: 30,
            processes: 128,
            browser_processes: 512,
            browser_session_memory_bytes: 2_147_483_648,
            browser_session_cpu_seconds: 300,
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
        let system = PathBuf::from("/usr/bin/bwrap");
        let local = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
            .map(|p| p.join("werk-worker/tools/bubblewrap/usr/bin/bwrap"));
        Self {
            bubblewrap: if system.is_file() {
                system
            } else {
                local.filter(|p| p.is_file()).unwrap_or(system)
            },
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
        self.run_observed(workspace, writable, argv, cwd, cancel, None, false, None)
            .await
    }
    /// Supply literal input to a single sandboxed command. EOF is delivered
    /// after the bounded input; cancellation and resource limits still apply.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with_stdin(
        &self,
        workspace: &Workspace,
        writable: bool,
        argv: &[String],
        cwd: &str,
        cancel: CancellationToken,
        stdin: &str,
    ) -> Result<ProcessOutput> {
        if stdin.len() > crate::process_input::STDIN_LIMIT {
            return Err(WorkerError::new(
                "invalid_stdin",
                "Standard input exceeds 32768 bytes; use a workspace script for larger programs",
            ));
        }
        self.run_observed(
            workspace,
            writable,
            argv,
            cwd,
            cancel,
            None,
            false,
            Some(stdin),
        )
        .await
    }
    pub(crate) async fn run_browser(
        &self,
        workspace: &Workspace,
        argv: &[String],
        cancel: CancellationToken,
        script: &str,
    ) -> Result<ProcessOutput> {
        // Bound expensive Chromium launches across main/child engines in this
        // process. Waiting is cancellable and consumes no inference request.
        static BROWSER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _permit = tokio::select! {
            permit = BROWSER.lock() => permit,
            _ = cancel.cancelled() => return Err(WorkerError::new("cancelled", "Browser wait cancelled")),
        };
        self.run_observed(
            workspace,
            false,
            argv,
            ".",
            cancel,
            None,
            true,
            Some(script),
        )
        .await
    }
    /// Browser broker uses an empty workspace and private network namespace.
    #[cfg(target_os = "linux")]
    pub(crate) async fn browser_command(&self) -> Result<tokio::process::Command> {
        let mut browser = self.clone();
        browser.limits.cpu_seconds = self.limits.browser_session_cpu_seconds;
        browser.command(None, false, &[], ".", true, true).await
    }

    #[cfg(target_os = "linux")]
    async fn command(
        &self,
        workspace: Option<&Workspace>,
        writable: bool,
        argv: &[String],
        cwd: &str,
        browser: bool,
        pipe_stdin: bool,
    ) -> Result<tokio::process::Command> {
        relative_path(cwd)?;
        if let Some(workspace) = workspace {
            workspace.revalidate().await?;
        }
        let mut command = tokio::process::Command::new(&self.bubblewrap);
        command
            .env_clear()
            .stdin(if pipe_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
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
        let workspace_fd = workspace.map(|w| w.root.as_raw_fd());
        if let Some(fd) = workspace_fd {
            command
                .arg(if writable {
                    "--bind-fd"
                } else {
                    "--ro-bind-fd"
                })
                .arg(fd.to_string())
                .arg("/workspace");
        } else {
            command.args(["--dir", "/workspace"]);
        }
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
        ]);
        if browser {
            let runtime = crate::browser::runtime_path().ok_or_else(|| {
                WorkerError::new(
                    "browser_unavailable",
                    "Install the WW browser runtime with scripts/install-browser.sh",
                )
            })?;
            command
                .args(["--ro-bind"])
                .arg(runtime)
                .arg("/opt/werk-browser");
            command.args([
                "--ro-bind-try",
                "/etc/fonts",
                "/etc/fonts",
                "--setenv",
                "PYTHONPATH",
                "/opt/werk-browser/python",
                "--setenv",
                "PLAYWRIGHT_BROWSERS_PATH",
                "/opt/werk-browser/browsers",
            ]);
            // Optional private library bundle, installed by the host. No model
            // arguments can choose loader paths or executable directories.
            #[cfg(target_arch = "x86_64")]
            let multiarch = "x86_64-linux-gnu";
            #[cfg(target_arch = "aarch64")]
            let multiarch = "aarch64-linux-gnu";
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            command.args([
                "--setenv", "LD_LIBRARY_PATH",
                &format!("/opt/werk-browser/deps/usr/lib/{multiarch}:/opt/werk-browser/deps/lib/{multiarch}"),
                "--setenv", "XDG_DATA_DIRS", "/opt/werk-browser/deps/usr/share:/usr/share",
                "--setenv", "GIO_EXTRA_MODULES", &format!("/opt/werk-browser/deps/usr/lib/{multiarch}/gio/modules"),
                "--setenv", "GST_PLUGIN_PATH_1_0", &format!("/opt/werk-browser/deps/usr/lib/{multiarch}/gstreamer-1.0"),
            ]);
        }
        if browser && workspace.is_none() {
            for path in crate::browser_control::BROWSER_INSTALL_DIRS {
                if std::path::Path::new(path).is_dir() {
                    command.args(["--ro-bind", path, path]);
                }
            }
        }
        if browser && std::env::var("WERK_WORKER_BROWSER_DEBUG").as_deref() == Ok("1") {
            command.args(["--setenv", "DEBUG", "pw:browser"]);
        }
        command.arg("--");
        command.args(argv);
        let limits = self.limits.clone();
        // RLIMIT_NPROC is charged to the real host UID, including threads,
        // not to the PID namespace. Reserve a bounded allowance above the
        // current UID baseline. This is a UID-wide ceiling, not a cgroup.
        let process_ceiling = host_task_baseline().await?.saturating_add(if browser {
            limits.browser_processes
        } else {
            limits.processes
        });
        // SAFETY: only async-signal-safe libc calls before exec; no allocation/locks.
        unsafe {
            command.pre_exec(move || {
                // Do not inherit the embedding host's control sockets/pipes.
                // Only the pinned workspace descriptor is passed to bwrap,
                // which consumes it for mounting and closes it before exec.
                if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if workspace_fd.is_some_and(|fd| libc::fcntl(fd, libc::F_SETFD, 0) < 0) {
                    return Err(std::io::Error::last_os_error());
                }
                if browser {
                    // Keep software rendering from occupying the host CPU.
                    let mut allowed: libc::cpu_set_t = std::mem::zeroed();
                    let mut bounded: libc::cpu_set_t = std::mem::zeroed();
                    if libc::sched_getaffinity(
                        0,
                        std::mem::size_of::<libc::cpu_set_t>(),
                        &mut allowed,
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    let mut count = 0;
                    for cpu in 0..libc::CPU_SETSIZE as usize {
                        if libc::CPU_ISSET(cpu, &allowed) {
                            libc::CPU_SET(cpu, &mut bounded);
                            count += 1;
                            if count == 2 {
                                break;
                            }
                        }
                    }
                    if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &bounded)
                        != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                for (resource, value) in [
                    // Chromium/V8 reserve large *virtual* address arenas.
                    // Browser RSS is independently bounded below.
                    (
                        libc::RLIMIT_AS,
                        if browser {
                            64 * 1024_u64.pow(4)
                        } else {
                            limits.memory_bytes
                        },
                    ),
                    (libc::RLIMIT_CPU, limits.cpu_seconds),
                    (libc::RLIMIT_NPROC, process_ceiling),
                    (libc::RLIMIT_FSIZE, 16 * 1024 * 1024),
                    // Firefox's per-tab IPC brokers need more than 128 FDs.
                    (libc::RLIMIT_NOFILE, if browser { 1024 } else { 128 }),
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
        Ok(command)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_observed(
        &self,
        workspace: &Workspace,
        writable: bool,
        argv: &[String],
        cwd: &str,
        cancel: CancellationToken,
        live: Option<Arc<LiveOutput>>,
        browser: bool,
        stdin: Option<&str>,
    ) -> Result<ProcessOutput> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (workspace, writable, argv, cwd, cancel, live, browser, stdin);
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
            let mut command = self
                .command(
                    Some(workspace),
                    writable,
                    argv,
                    cwd,
                    browser,
                    stdin.is_some(),
                )
                .await?;
            let mut child = command.spawn().map_err(|_| {
                WorkerError::new(
                    "sandbox_unavailable",
                    "Cannot start bubblewrap; no host fallback",
                )
            })?;
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            let input = child.stdin.take();
            // Poll input concurrently with process completion and output drains.
            // Dropping this future closes the pipe on exit/timeout/cancellation;
            // a child that never reads cannot leave a detached writer behind.
            let input_task = async move {
                if let Some(mut pipe) = input {
                    let result = pipe.write_all(stdin.unwrap_or_default().as_bytes()).await;
                    drop(pipe);
                    if let Err(error) = result
                        && error.kind() != std::io::ErrorKind::BrokenPipe
                    {
                        return Err(WorkerError::new(
                            "process_io",
                            "Failed to deliver standard input",
                        ));
                    }
                }
                Ok::<(), WorkerError>(())
            };
            tokio::pin!(input_task);
            let out_task = tokio::spawn(drain_live(
                stdout,
                self.limits.output_bytes,
                live.clone(),
                false,
            ));
            let err_task = tokio::spawn(drain_live(stderr, self.limits.output_bytes, live, true));
            let mut cancelled = false;
            let mut timed_out = false;
            let child_pid = child.id().unwrap_or(0);
            let status = {
                let status_task = async {
                    Ok::<_, WorkerError>(tokio::select! {
                        s = child.wait() => s?,
                        _ = cancel.cancelled() => { cancelled=true; child.kill().await?; child.wait().await? },
                        _ = tokio::time::sleep(Duration::from_millis(self.limits.timeout_ms)) => { timed_out=true; child.kill().await?; child.wait().await? },
                        _ = browser_memory_limit(child_pid, self.limits.memory_bytes), if browser => { timed_out=true; child.kill().await?; child.wait().await? },
                    })
                };
                tokio::pin!(status_task);
                tokio::select! {
                    status = &mut status_task => status,
                    input_result = &mut input_task => {
                        // A failed input transport is not an application assertion.
                        // Still reap the bounded process and its output tasks.
                        let status = status_task.await;
                        input_result.and(status)
                    }
                }
            }?;
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
    live: Arc<LiveOutput>,
    finished: Arc<tokio::sync::Notify>,
    notified: bool,
}
#[derive(Default)]
struct LogBuffer {
    bytes: Vec<u8>,
    total: usize,
}
#[derive(Default)]
struct LiveOutput {
    stdout: Mutex<LogBuffer>,
    stderr: Mutex<LogBuffer>,
}

async fn drain_live<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
    live: Option<Arc<LiveOutput>>,
    stderr: bool,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut result = Vec::new();
    let mut truncated = false;
    let mut chunk = [0; 8192];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if let Some(live) = &live {
            let mut log = if stderr {
                live.stderr.lock().await
            } else {
                live.stdout.lock().await
            };
            log.total += n;
            log.bytes.extend_from_slice(&chunk[..n]);
            let excess = log.bytes.len().saturating_sub(65536);
            if excess > 0 {
                log.bytes.drain(..excess);
            }
        }
        let keep = n.min(limit.saturating_sub(result.len()));
        result.extend_from_slice(&chunk[..keep]);
        truncated |= keep < n;
    }
    Ok((result, truncated))
}

pub(crate) async fn browser_memory_limit(pid: u32, limit: u64) {
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let rss = tokio::task::spawn_blocking(move || {
            let mut pending = vec![pid];
            let mut seen = std::collections::BTreeSet::new();
            let mut pages = 0_u64;
            while let Some(pid) = pending.pop() {
                if !seen.insert(pid) || seen.len() > 512 {
                    continue;
                }
                if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/statm")) {
                    pages += s
                        .split_whitespace()
                        .nth(1)
                        .and_then(|n| n.parse::<u64>().ok())
                        .unwrap_or(0);
                }
                // Chromium spawns subprocesses from multiple threads. A single
                // /task/<leader>/children would miss those descendants.
                if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
                    for task in tasks.flatten() {
                        if let Ok(s) = std::fs::read_to_string(task.path().join("children")) {
                            pending
                                .extend(s.split_whitespace().filter_map(|n| n.parse::<u32>().ok()));
                        }
                    }
                }
            }
            // Linux supported architectures use a host-provided page size.
            #[cfg(unix)]
            let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
            #[cfg(not(unix))]
            let size = 4096;
            pages.saturating_mul(size)
        })
        .await
        .unwrap_or(u64::MAX);
        if rss > limit {
            return;
        }
    }
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
        let live = Arc::new(LiveOutput::default());
        let log = live.clone();
        let finished = Arc::new(tokio::sync::Notify::new());
        let wake = finished.clone();
        let task = tokio::spawn(async move {
            let _lease = lease;
            let result = sandbox
                .run_observed(
                    &workspace,
                    writable,
                    &argv,
                    &cwd,
                    child_cancel,
                    Some(log),
                    false,
                    None,
                )
                .await;
            // A finished notification also promises the workspace is released.
            drop(_lease);
            *sink.lock().await = Some(result);
            wake.notify_waiters();
        });
        jobs.insert(
            job_id.clone(),
            Job {
                run_id: run_id.into(),
                cancel,
                output,
                task: Some(task),
                live,
                finished,
                notified: false,
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
    pub async fn wait(
        &self,
        run_id: &str,
        job_id: &str,
        timeout: u64,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let (finished, output) = {
            let jobs = self.jobs.lock().await;
            let job = jobs
                .get(job_id)
                .filter(|j| j.run_id == run_id)
                .ok_or_else(|| {
                    WorkerError::new("unknown_job", "Job does not belong to this run")
                })?;
            (job.finished.clone(), job.output.clone())
        };
        let wake = finished.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        if output.lock().await.is_none() {
            tokio::select! { _ = &mut wake => {}, _ = tokio::time::sleep(Duration::from_millis(timeout.min(30000))) => {}, _ = cancel.cancelled() => return Err(WorkerError::new("cancelled","Job wait cancelled")) }
        }
        let mut status = self.status(run_id, job_id, false).await?;
        if let Some(output) = status.get_mut("output").and_then(Value::as_object_mut) {
            output.remove("stdout");
            output.remove("stderr");
            output.insert(
                "hint".into(),
                json!("Use process.log for bounded stdout/stderr pages"),
            );
        }
        Ok(status)
    }
    pub async fn log(
        &self,
        run_id: &str,
        job_id: &str,
        stream: &str,
        offset: Option<usize>,
        length: usize,
    ) -> Result<Value> {
        let live = {
            let jobs = self.jobs.lock().await;
            jobs.get(job_id)
                .filter(|j| j.run_id == run_id)
                .map(|j| j.live.clone())
                .ok_or_else(|| WorkerError::new("unknown_job", "Job does not belong to this run"))?
        };
        let log = if stream == "stderr" {
            live.stderr.lock().await
        } else {
            live.stdout.lock().await
        };
        let dropped = log.total - log.bytes.len();
        let mut start = offset
            .unwrap_or(log.total.saturating_sub(length.min(4096)))
            .clamp(dropped, log.total)
            - dropped;
        while start < log.bytes.len() && log.bytes[start] & 0xc0 == 0x80 {
            start += 1;
        }
        let mut end = (start + length.min(4096)).min(log.bytes.len());
        if let Err(e) = std::str::from_utf8(&log.bytes[start..end])
            && e.error_len().is_none()
        {
            end = start + e.valid_up_to();
            // A tiny page must advance over one complete UTF-8 scalar.
            if end == start {
                end = (start + 1).min(log.bytes.len());
                while end < log.bytes.len() && log.bytes[end] & 0xc0 == 0x80 {
                    end += 1;
                }
            }
        }
        Ok(
            json!({"job_id":job_id,"stream":stream,"offset":start+dropped,"next_offset":end+dropped,"total_bytes":log.total,"dropped_before":dropped,"text":String::from_utf8_lossy(&log.bytes[start..end])}),
        )
    }
    pub(crate) async fn completed(&self, run_id: &str) -> Vec<Value> {
        let mut jobs = self.jobs.lock().await;
        let mut events = Vec::new();
        for (id, job) in jobs
            .iter_mut()
            .filter(|(_, j)| j.run_id == run_id && !j.notified)
        {
            let output = job.output.lock().await;
            if let Some(out) = &*output {
                events.push(match out {Ok(out)=>json!({"job_id":id,"exit_code":out.exit_code,"cancelled":out.cancelled,"timed_out":out.timed_out}),Err(e)=>json!({"job_id":id,"error":e.code})});
                job.notified = true;
            }
        }
        events
    }
    pub(crate) async fn running(&self, run_id: &str) -> Vec<String> {
        let jobs = self.jobs.lock().await;
        let mut ids = Vec::new();
        for (id, job) in jobs.iter().filter(|(_, j)| j.run_id == run_id) {
            if job.output.lock().await.is_none() {
                ids.push(id.clone());
            }
        }
        ids
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
