//! Explicitly selected, fork-denied Seatbelt execution on macOS 15 or newer.
//!
//! Seatbelt path filters cannot pin a workspace vnode like Linux bind-fd. The
//! trusted host must accept the external-rename assumption before using this
//! mode. No project input can select it. A separate, libc-only supervisor owns
//! the one untrusted process and survives the embedding host's exit.
use super::{LiveOutput, ProcessOutput, Sandbox, Workspace, drain_live};
use crate::{core::*, policy::relative_path};
use serde_json::{Value, json};
use std::{
    ffi::CString,
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{ffi::OsStrExt, fs::DirBuilderExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

pub(super) const BACKEND: &str = "seatbelt-single-process";
const EXECUTABLE: &str = "/usr/bin/sandbox-exec";
const PROFILE_VERSION: u32 = 6;
const POLL_MS: i32 = 10;

// Darwin Mach ABI, supplied by libSystem. Keep these bindings local: libc does
// not expose the task-port routines. No library is loaded from the workspace.
unsafe extern "C" {
    fn mach_port_kobject(task: u32, name: u32, kind: *mut u32, address: *mut u64) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
    fn mach_ports_register(task: u32, ports: *mut u32, count: u32) -> libc::c_int;
    fn task_get_special_port(task: u32, which: libc::c_int, port: *mut u32) -> libc::c_int;
    fn task_set_special_port(task: u32, which: libc::c_int, port: u32) -> libc::c_int;
    fn task_set_exception_ports(task: u32, mask: u32, port: u32, behavior: i32, flavor: i32)
    -> i32;
    fn task_get_exception_ports(
        task: u32,
        mask: u32,
        masks: *mut u32,
        count: *mut u32,
        ports: *mut u32,
        behaviors: *mut i32,
        flavors: *mut i32,
    ) -> i32;
}
// mach/task_special_ports.h: these selectors are not kernel object types.
// Selector 3 is TASK_NAME_PORT; IKOT_HOST below is a different enum.
const TASK_HOST_PORT: i32 = 2;
const TASK_BOOTSTRAP_PORT: i32 = 4;
#[cfg(test)]
const TASK_ACCESS_PORT: i32 = 9;
#[cfg(test)]
const KERN_DENIED: i32 = 53;
const EXCEPTION_MASK: u32 = 0x3ffe; // all Darwin exceptions 1..=13, including crash/corpse
const IKOT_HOST: u32 = 3;

// These tags also identify setup failures in the supervisor's fixed-size report.
#[derive(Debug)]
struct MachSetupError {
    stage: u8,
    detail: i32,
}

impl MachSetupError {
    fn into_worker_error(self) -> WorkerError {
        let operation = match self.stage {
            7 => "host-port query",
            8 => "host-port kernel inspection",
            9 => "host-port object type",
            10 => "host-port reference release",
            15 => "registered-port clearing",
            16 => "bootstrap query",
            17 => "bootstrap clearing",
            18 => "bootstrap clearing verification",
            19 => "exception-port query",
            20 => "exception-port clearing",
            21 => "exception-port verification query",
            22 => "exception query did not cover the complete mask",
            23 => "exception ports remain after clearing",
            _ => "unknown Mach setup operation",
        };
        WorkerError::new(
            "sandbox_requirement_unavailable",
            format!(
                "Native launcher could not establish isolated Mach ports \
                 (stage={}, detail={}; operation={operation}; detail is the immediate Mach result, port name, object type or exception mask)",
                self.stage, self.detail,
            ),
        )
    }
}

unsafe fn inspect_host_port(task: u32, port: u32) -> std::result::Result<(), MachSetupError> {
    let mut kind = u32::MAX;
    let mut address = 0_u64;
    // Ask the kernel through our task port, never the possibly hostile host
    // endpoint. XNU exposes the object type on release kernels too; the kernel
    // address is deliberately ignored. IKOT_NONE (userspace), IKOT_HOST_PRIV,
    // missing/dead rights, and failed queries are all rejected.
    let status = unsafe { mach_port_kobject(task, port, &mut kind, &mut address) };
    if status != 0 {
        return Err(MachSetupError {
            stage: 8,
            detail: status,
        });
    }
    if kind != IKOT_HOST {
        return Err(MachSetupError {
            stage: 9,
            detail: kind as i32,
        });
    }
    Ok(())
}

unsafe fn verify_host_port() -> std::result::Result<(), MachSetupError> {
    #[allow(deprecated)]
    let task = unsafe { libc::mach_task_self() };
    let mut port = 0;
    let status = unsafe { task_get_special_port(task, TASK_HOST_PORT, &mut port) };
    if status != 0 {
        return Err(MachSetupError {
            stage: 7,
            detail: status,
        });
    }
    let result = unsafe { inspect_host_port(task, port) };
    if port != 0 {
        let status = unsafe { mach_port_deallocate(task, port) };
        if status != 0 {
            return Err(MachSetupError {
                stage: 10,
                detail: status,
            });
        }
    }
    result
}

// All varying paths are -D parameters, not Scheme source or a shell string.
// No mach-lookup, network, shared-memory, IOKit or fork permission is granted.
// Explicitly deny special-port extraction/replacement, including target self.
// TASK_ACCESS_PORT cannot be revoked and is required to remain kernel-only.
// Native conformance is mandatory: on macOS 26.6.2 the getter rule still exposes
// that slot. See docs/cross-platform-sandbox.md for the unresolved isolation gate.
// Apple uses these operations on macOS >=14; unknown ones fail compilation.
// dyld/libignition opens the root directory during startup. Permit only that
// directory's entries, never a root subpath grant or descendant file contents.
// libc's getpagesize/sysconf uses hw.pagesize_compat on Apple Silicon. Denying
// it breaks Rust's guard-page setup before main, even with hw.pagesize allowed.
const PROFILE: &str = r#"(version 1)
(deny default)
(deny mach-task-special-port-get)
(deny mach-task-special-port-set)
(allow process-exec)
(deny process-fork)
(allow signal (target self))
(allow process-info-pidinfo (target self))
(allow sysctl-read
  (sysctl-name "hw.ncpu" "hw.logicalcpu" "hw.physicalcpu" "hw.pagesize"
               "hw.pagesize_compat"
               "kern.osrelease" "kern.ostype" "kern.osversion" "kern.version"
               "sysctl.proc_translated"))
(allow file-read* file-map-executable
  (subpath "/System/Library") (subpath "/usr/lib")
  (subpath "/usr/share") (subpath "/bin") (subpath "/usr/bin")
  (subpath "/usr/libexec") (subpath "/Library/Developer/CommandLineTools")
  (subpath (param "WORKSPACE")) (subpath (param "PRIVATE_TMP")))
(allow file-read-metadata (literal "/") (literal "/private")
  (literal "/private/var") (literal "/private/tmp") (literal "/Users")
  (literal "/var") (literal "/tmp")
  (path-ancestors (param "PRIVATE_TMP")))
(if (not (string=? (param "WORKSPACE") "/"))
    (allow file-read-metadata (path-ancestors (param "WORKSPACE"))))
(allow file-read-data (literal "/")
  (literal "/dev/null") (literal "/dev/zero")
  (literal "/dev/random") (literal "/dev/urandom"))
(allow file-write-data (literal "/dev/null"))
(allow file-write* (subpath (param "PRIVATE_TMP")))
(if (string=? (param "WRITABLE") "true")
    (allow file-write* (subpath (param "WORKSPACE"))))
(deny file-write-unlink (literal (param "WORKSPACE")))
"#;

pub(super) fn requirements(sandbox: &Sandbox) -> Result<()> {
    if !sandbox.native.macos_path_based_workspace {
        return Err(WorkerError::new(
            "sandbox_requirement_unavailable",
            "macOS requires the trusted host to select macos_path_based_workspace; Seatbelt cannot inode-pin a subtree against external renames",
        ));
    }
    let limits = &sandbox.limits;
    if limits.timeout_ms == 0
        || limits.timeout_ms > i64::MAX as u64 / 1_000_000
        || limits.memory_bytes == 0
        || limits.memory_bytes >= libc::RLIM_INFINITY
        || limits.cpu_seconds == 0
        || limits.cpu_seconds >= libc::RLIM_INFINITY
        || limits.processes == 0
        || limits.output_bytes == 0
    {
        return Err(WorkerError::new(
            "resource_limit",
            "Invalid mandatory process limits",
        ));
    }
    if !cfg!(target_arch = "aarch64") {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "This native backend supports macOS Apple Silicon",
        ));
    }
    let mut system: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: uname writes exactly the provided utsname.
    if unsafe { libc::uname(&mut system) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let release = unsafe { std::ffi::CStr::from_ptr(system.release.as_ptr()) };
    let major = release
        .to_string_lossy()
        .split('.')
        .next()
        .and_then(|v| v.parse::<u32>().ok());
    if major.is_none_or(|major| major < 24) {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "Native Seatbelt execution requires macOS 15 or newer",
        ));
    }
    let launcher = std::fs::symlink_metadata(EXECUTABLE).map_err(|_| {
        WorkerError::new(
            "sandbox_unavailable",
            "The system sandbox-exec launcher is unavailable",
        )
    })?;
    if !launcher.is_file() || launcher.uid() != 0 || launcher.mode() & 0o022 != 0 {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "sandbox-exec must be a root-owned, non-writable system executable",
        ));
    }
    // Reject root/set-id launchers independently of the Mach port check.
    let (uid, euid, gid, egid) = unsafe {
        (
            libc::getuid(),
            libc::geteuid(),
            libc::getgid(),
            libc::getegid(),
        )
    };
    if euid == 0 || uid != euid || gid != egid {
        return Err(WorkerError::new(
            "sandbox_requirement_unavailable",
            format!(
                "Native execution requires an unprivileged host \
                 (uid={uid}, euid={euid}, gid={gid}, egid={egid})"
            ),
        ));
    }
    // Preflight for approval/doctor. The supervisor repeats this on its own
    // fork snapshot, closing the gap between preparation and actual launch.
    unsafe { verify_host_port() }.map_err(MachSetupError::into_worker_error)
}

pub(super) fn description(
    sandbox: &Sandbox,
    workspace: &Workspace,
    writable: bool,
) -> Result<Value> {
    requirements(sandbox)?;
    Ok(json!({
        "backend": BACKEND, "configuration_version": PROFILE_VERSION,
        "launcher": EXECUTABLE, "profile_sha256": hash(PROFILE.as_bytes()),
        "workspace_identity": workspace.identity, "workspace_path": workspace.path,
        "workspace_writable": writable, "cwd_namespace": "native-host-path",
        "workspace_binding": "path-filter-with-pinned-cwd-and-rename-monitor",
        "requires_trusted_external_workspace_renames": true,
        "network": "denied", "host_ipc": "denied", "fork": "denied",
        "inherited_mach_rights": "registered-bootstrap-exception-verified-empty-before-second-fork;task-access-kernel-slot-retained-with-special-port-get-set-denied;host-port-kernel-type-verified-after-fork",
        "host_requirements": "unprivileged-uid;host-port-IKOT_HOST-only",
        "untrusted_processes": 1,
        "readonly_runtime": ["/System/Library", "/usr/lib", "/usr/share", "/bin",
            "/usr/bin", "/usr/libexec", "/Library/Developer/CommandLineTools"],
        "path_metadata": {"system_aliases": ["/var", "/tmp"],
            "ancestors": ["workspace", "private-temporary-directory"]},
        "root_directory_entries": "readable-for-loader;no-descendant-access",
        "temporary_directory": "private-per-invocation",
        "environment": ["PATH=/usr/bin:/bin", "HOME=<private-temp>",
            "TMPDIR=<private-temp>", "LANG=C.UTF-8"],
        "resource_limits": sandbox.limits,
        "fixed_limits": {"file_bytes": 16 * 1024 * 1024,
            "open_files": 128, "core_bytes": 0},
        "cleanup": "single-child-supervisor-reaps-before-completion",
        "host_exit": "independent-supervisor-detects-parent-exit-or-lifeline-close",
        "supervisor_failure": "no-kernel-kill-on-close-guarantee",
        "api_status": "sandbox-exec-is-deprecated; native-probes-required"
    }))
}

struct PrivateTemp(PathBuf);
impl PrivateTemp {
    fn new() -> Result<Self> {
        let parent = std::fs::canonicalize(std::env::temp_dir())?;
        let path = parent.join(id("werk_process"));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for PrivateTemp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| WorkerError::new("path_denied", "NUL in native path"))
}

fn pipe() -> io::Result<(File, File)> {
    let mut descriptors = [-1; 2];
    // SAFETY: valid two-element output buffer. Both descriptors are owned here.
    if unsafe { libc::pipe(descriptors.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let originals = unsafe {
        [
            File::from_raw_fd(descriptors[0]),
            File::from_raw_fd(descriptors[1]),
        ]
    };
    let mut copies = Vec::with_capacity(2);
    for file in &originals {
        // Reserve 0/1/2 exclusively for the final dup2, including when an
        // embedding host entered with one of its standard streams closed.
        let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        copies.push(unsafe { File::from_raw_fd(fd) });
    }
    let write = copies.pop().unwrap();
    let read = copies.pop().unwrap();
    Ok((read, write))
}

struct Supervised {
    pid: libc::pid_t,
    control: Option<File>,
    waited: bool,
    private: Option<PrivateTemp>,
}
impl Supervised {
    fn stop(&mut self) {
        self.control.take();
    }
    async fn reap(&mut self) -> Result<()> {
        let pid = self.pid;
        tokio::task::spawn_blocking(move || reap_pid(pid))
            .await
            .map_err(|_| WorkerError::new("cleanup_failed", "Supervisor wait failed"))??;
        self.waited = true;
        self.private.take();
        Ok(())
    }
}
impl Drop for Supervised {
    fn drop(&mut self) {
        self.stop();
        if !self.waited {
            let pid = self.pid;
            let private = self.private.take();
            // Closing the control pipe makes the still-live supervisor kill and
            // reap its child. Do not kill the supervisor and orphan the child.
            let _ = std::thread::Builder::new()
                .name("werk-macos-reap".into())
                .spawn(move || {
                    let _ = reap_pid(pid);
                    drop(private);
                });
        }
    }
}

fn reap_pid(pid: libc::pid_t) -> io::Result<()> {
    loop {
        let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        if result == pid {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[derive(Clone, Copy)]
struct Identity {
    device: libc::dev_t,
    inode: libc::ino_t,
}
impl Identity {
    fn of(file: &File) -> io::Result<Self> {
        let m = file.metadata()?;
        Ok(Self {
            device: m.dev() as libc::dev_t,
            inode: m.ino(),
        })
    }
}

// All functions below are called after fork in a multithreaded embedding host.
// They use only preallocated data and libc syscalls: no allocation, locks,
// formatting, Rust destruction, Tokio, Objective-C, or sandbox_init.
unsafe fn same_object(path: &CString, identity: Identity) -> bool {
    let mut info: libc::stat = unsafe { std::mem::zeroed() };
    let exists = unsafe { libc::lstat(path.as_ptr(), &mut info) == 0 };
    exists
        && info.st_dev == identity.device
        && info.st_ino == identity.inode
        && info.st_mode & libc::S_IFMT == libc::S_IFDIR
}

unsafe fn close_except(buffer: &mut [libc::proc_fdinfo], keep: &[RawFd]) -> bool {
    // getdtablesize/RLIMIT_NOFILE are insufficient: the host can retain a
    // descriptor above a subsequently lowered limit. Enumerate this now
    // single-threaded fork snapshot instead. Apple's libproc implementation
    // of proc_pidinfo is only a __proc_info syscall wrapper; resolve it in the
    // parent before fork, and supply all storage in advance.
    let capacity = std::mem::size_of_val(buffer) as libc::c_int;
    let bytes = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDLISTFDS,
            0,
            buffer.as_mut_ptr().cast(),
            capacity,
        )
    };
    if bytes <= 0 || bytes >= capacity || bytes % libc::PROC_PIDLISTFD_SIZE != 0 {
        return false;
    }
    for entry in &buffer[..(bytes / libc::PROC_PIDLISTFD_SIZE) as usize] {
        let fd = entry.proc_fd;
        if !keep.contains(&fd) && unsafe { libc::close(fd) } != 0 {
            // Do not guess whether a failed close left a host handle open.
            return false;
        }
    }
    true
}

unsafe fn raw_write(fd: RawFd, bytes: &[u8]) -> bool {
    let mut offset = 0;
    while offset < bytes.len() {
        let written =
            unsafe { libc::write(fd, bytes[offset..].as_ptr().cast(), bytes.len() - offset) };
        if written > 0 {
            offset += written as usize;
        } else if written < 0 && unsafe { *libc::__error() } == libc::EINTR {
            continue;
        } else {
            return false;
        }
    }
    true
}

unsafe fn setup_failed(fd: RawFd) -> ! {
    unsafe {
        raw_write(fd, &[1]);
    }
    unsafe { libc::_exit(125) }
}

unsafe fn monotonic_ms() -> i64 {
    let mut now: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
        return -1;
    }
    now.tv_sec
        .saturating_mul(1000)
        .saturating_add(now.tv_nsec / 1_000_000)
}

// XNU ipc_task_init starts a NEW IPC namespace on fork, then inherits selected
// task ports. Clear those on the supervisor before its child is forked: merely
// clearing the slot immediately before exec would leave already-copied-out
// send-right names in the child's namespace. The normal kernel host port stays
// available only after checking its kernel object type in this fork snapshot.
// Darwin's Mach stubs use stack messages and kernel IPC; mach_ports_register is
// a fixed three-port wrapper. All symbols are resolved before the first fork.
fn mach_result(status: i32, stage: u8) -> std::result::Result<(), MachSetupError> {
    if status == 0 {
        Ok(())
    } else {
        Err(MachSetupError {
            stage,
            detail: status,
        })
    }
}

// Query the complete exception mask, returning the subset with non-null ports.
// The second fork discards any names copied out by these queries. Even a dead
// name must be cleared from its slot before that fork.
unsafe fn exception_ports_present(
    task: u32,
    stage: u8,
) -> std::result::Result<u32, MachSetupError> {
    let mut masks = [0_u32; 32];
    let mut ports = [0_u32; 32];
    let mut behaviors = [0_i32; 32];
    let mut flavors = [0_i32; 32];
    let mut count = 32;
    let status = unsafe {
        task_get_exception_ports(
            task,
            EXCEPTION_MASK,
            masks.as_mut_ptr(),
            &mut count,
            ports.as_mut_ptr(),
            behaviors.as_mut_ptr(),
            flavors.as_mut_ptr(),
        )
    };
    mach_result(status, stage)?;
    if count > 32 {
        return Err(MachSetupError {
            stage: 22,
            detail: count as i32,
        });
    }
    let mut covered = 0;
    let mut present = 0;
    for i in 0..count as usize {
        covered |= masks[i];
        if ports[i] != 0 {
            present |= masks[i];
        }
    }
    if covered != EXCEPTION_MASK {
        return Err(MachSetupError {
            stage: 22,
            detail: covered as i32,
        });
    }
    Ok(present)
}

unsafe fn scrub_inherited_mach() -> std::result::Result<(), MachSetupError> {
    #[allow(deprecated)] // local libSystem ABI avoids a second Mach binding dependency
    let task = unsafe { libc::mach_task_self() };
    // launchd normally supplies a live TASK_ACCESS_PORT for kernel code-signing
    // upcalls. XNU refuses to replace it. Do not copy it into this namespace or
    // pretend that deallocating a name revokes the kernel's retained slot.
    // The next fork discards copied-out names; PROFILE denies the user getter
    // and setter before untrusted code, while kernel-internal upcalls remain.
    let mut port = 0_u32;
    mach_result(
        unsafe { mach_ports_register(task, std::ptr::null_mut(), 0) },
        15,
    )?;
    mach_result(
        unsafe { task_get_special_port(task, TASK_BOOTSTRAP_PORT, &mut port) },
        16,
    )?;
    if port != 0 {
        mach_result(
            unsafe { task_set_special_port(task, TASK_BOOTSTRAP_PORT, 0) },
            17,
        )?;
        mach_result(
            unsafe { task_get_special_port(task, TASK_BOOTSTRAP_PORT, &mut port) },
            18,
        )?;
        if port != 0 {
            return Err(MachSetupError {
                stage: 18,
                detail: port as i32,
            });
        }
    }
    let present = unsafe { exception_ports_present(task, 19) }?;
    if present != 0 {
        // Change only populated slots. Already empty slots require no mutation
        // authority, and querying all slots still proves the required boundary.
        mach_result(
            unsafe { task_set_exception_ports(task, present, 0, 1, 0) },
            20,
        )?;
        // MAC labels can prevent individual actions from changing even when
        // the setter succeeds. Re-query the complete mask and fail closed.
        let remaining = unsafe { exception_ports_present(task, 21) }?;
        if remaining != 0 {
            return Err(MachSetupError {
                stage: 23,
                detail: remaining as i32,
            });
        }
    }
    Ok(())
}

fn resolve_mach_symbols() {
    // Invalid destination 0 resolves the MIG stubs without changing host state.
    let mut value = 0;
    let mut count = 1;
    let mut mask = 0;
    let mut behavior = 0;
    let mut flavor = 0;
    unsafe {
        let mut address = 0;
        mach_port_kobject(0, 0, &mut value, &mut address);
        mach_port_deallocate(0, 0);
        mach_ports_register(0, std::ptr::null_mut(), 0);
        task_get_special_port(0, TASK_BOOTSTRAP_PORT, &mut value);
        task_set_special_port(0, TASK_BOOTSTRAP_PORT, 0);
        task_set_exception_ports(0, EXCEPTION_MASK, 0, 1, 0);
        task_get_exception_ports(
            0,
            EXCEPTION_MASK,
            &mut mask,
            &mut count,
            &mut value,
            &mut behavior,
            &mut flavor,
        );
    }
}

struct SpawnData {
    executable: CString,
    arguments: Vec<CString>,
    environment: Vec<CString>,
    root_path: CString,
    cwd_path: CString,
    root_identity: Identity,
    cwd_identity: Identity,
    cwd_fd: RawFd,
    limits: super::ProcessLimits,
    descriptors: Vec<libc::proc_fdinfo>,
}

#[allow(clippy::too_many_arguments)] // explicit raw descriptors at the fork boundary
unsafe fn supervisor(
    data: &mut SpawnData,
    argv: &[*const libc::c_char],
    env: &[*const libc::c_char],
    host_pid: libc::pid_t,
    input: RawFd,
    out: RawFd,
    err: RawFd,
    control: RawFd,
    report: RawFd,
    setup_read: RawFd,
    setup_write: RawFd,
) -> ! {
    let mut empty: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
        for signal in 1..32 {
            if signal != libc::SIGKILL && signal != libc::SIGSTOP {
                libc::signal(signal, libc::SIG_DFL);
            }
        }
        if libc::setsid() < 0 {
            libc::_exit(125);
        }
    }
    // parent-death monitoring also closes the small pipe()/FD_CLOEXEC race
    // with an unrelated embedding-host spawn inheriting the lifeline writer.
    if unsafe { libc::getppid() } != host_pid
        || !unsafe { same_object(&data.root_path, data.root_identity) }
        || !unsafe { same_object(&data.cwd_path, data.cwd_identity) }
    {
        unsafe { libc::_exit(125) }
    }
    // Prevent a closed parent report reader from killing the supervisor before
    // it has reaped its child. Reset this in the untrusted process before exec.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    if let Err(error) = unsafe { verify_host_port() } {
        let mut result = [0_u8; 5];
        result[0] = error.stage;
        result[1..].copy_from_slice(&error.detail.to_ne_bytes());
        unsafe {
            raw_write(report, &result);
            libc::_exit(125)
        }
    }
    if let Err(error) = unsafe { scrub_inherited_mach() } {
        let mut result = [0_u8; 5];
        result[0] = error.stage;
        result[1..].copy_from_slice(&error.detail.to_ne_bytes());
        unsafe {
            raw_write(report, &result);
            libc::_exit(125)
        }
    }
    let child = unsafe { libc::fork() };
    if child < 0 {
        unsafe { libc::_exit(125) }
    }
    if child == 0 {
        if unsafe { libc::fchdir(data.cwd_fd) } != 0
            || unsafe { libc::dup2(input, 0) } < 0
            || unsafe { libc::dup2(out, 1) } < 0
            || unsafe { libc::dup2(err, 2) } < 0
        {
            unsafe { setup_failed(setup_write) }
        }
        if !unsafe { close_except(&mut data.descriptors, &[0, 1, 2, setup_write]) } {
            unsafe { setup_failed(setup_write) }
        }
        // Reset inherited signal state rather than leaking host callbacks or a
        // blocked SIGXCPU. A fixed signal set avoids querying untrusted input.
        let mut empty: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut empty);
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            for signal in 1..32 {
                if signal != libc::SIGKILL && signal != libc::SIGSTOP {
                    libc::signal(signal, libc::SIG_DFL);
                }
            }
        }
        for (resource, value) in [
            (libc::RLIMIT_AS, data.limits.memory_bytes),
            (libc::RLIMIT_CPU, data.limits.cpu_seconds),
            (libc::RLIMIT_FSIZE, 16 * 1024 * 1024),
            (libc::RLIMIT_NOFILE, 128),
            (libc::RLIMIT_CORE, 0),
        ] {
            let limit = libc::rlimit {
                rlim_cur: value,
                rlim_max: value,
            };
            if unsafe { libc::setrlimit(resource, &limit) } != 0 {
                unsafe { setup_failed(setup_write) }
            }
        }
        // sandbox-exec installs Seatbelt before it runs the requested program.
        // Neither an inherited shell nor a host environment is involved.
        unsafe {
            libc::execve(data.executable.as_ptr(), argv.as_ptr(), env.as_ptr());
        }
        unsafe { setup_failed(setup_write) }
    }
    if !unsafe { close_except(&mut data.descriptors, &[control, report, setup_read]) } {
        unsafe {
            libc::kill(child, libc::SIGKILL);
        }
        while unsafe { libc::waitpid(child, std::ptr::null_mut(), 0) } < 0
            && unsafe { *libc::__error() } == libc::EINTR
        {}
        unsafe { libc::_exit(125) }
    }
    let start = unsafe { monotonic_ms() };
    let mut status = 0;
    let mut reason = 0_u8;
    loop {
        // Keep the child unreaped until the final wait below, preventing PID
        // reuse while cancellation may still send SIGKILL to that PID.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let observed = unsafe {
            libc::waitid(
                libc::P_PID,
                child as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if observed < 0 {
            if unsafe { *libc::__error() } == libc::EINTR {
                continue;
            }
            reason = 4;
            break;
        }
        if unsafe { info.si_pid() } == child {
            break;
        }
        if !unsafe { same_object(&data.root_path, data.root_identity) }
            || !unsafe { same_object(&data.cwd_path, data.cwd_identity) }
        {
            reason = 3;
            break;
        }
        let now = unsafe { monotonic_ms() };
        if start < 0 || now < start {
            reason = 4;
            break;
        }
        if now - start >= data.limits.timeout_ms as i64 {
            reason = 2;
            break;
        }
        if unsafe { libc::getppid() } != host_pid {
            reason = 1;
            break;
        }
        let mut descriptor = libc::pollfd {
            fd: control,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, POLL_MS) };
        if result > 0 {
            reason = 1;
            break;
        }
        if result < 0 && unsafe { *libc::__error() } != libc::EINTR {
            reason = 4;
            break;
        }
    }
    if reason != 0 {
        unsafe {
            libc::kill(child, libc::SIGKILL);
        }
    }
    loop {
        let waited = unsafe { libc::waitpid(child, &mut status, 0) };
        if waited == child {
            break;
        }
        if waited < 0 && unsafe { *libc::__error() } == libc::EINTR {
            continue;
        }
        reason = 4;
        break;
    }
    // The setup writer was CLOEXEC and not given to the program. Distinguish
    // a mandatory pre-exec setup failure from an application exiting 125.
    let mut setup = 0_u8;
    if unsafe { libc::read(setup_read, (&mut setup as *mut u8).cast(), 1) } > 0 {
        reason = 5;
    }
    let mut result = [0_u8; 5];
    result[0] = reason;
    result[1..].copy_from_slice(&status.to_ne_bytes());
    unsafe {
        raw_write(report, &result);
    }
    unsafe { libc::_exit(0) }
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
) -> Result<ProcessOutput> {
    requirements(sandbox)?;
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
    let directory = super::workspace_macos::Opened::open(&workspace.root, cwd, true)?;
    directory.validate()?;
    let root_path = super::workspace_macos::descriptor_path(&workspace.root)?;
    let cwd_path = super::workspace_macos::descriptor_path(&directory.file)?;
    let private = PrivateTemp::new()?;
    let root_text = root_path.to_str().ok_or_else(|| {
        WorkerError::new("path_denied", "Seatbelt requires a UTF-8 workspace path")
    })?;
    let temp_text = private.0.to_str().ok_or_else(|| {
        WorkerError::new("path_denied", "Seatbelt requires a UTF-8 temporary path")
    })?;
    let mut arguments = vec![
        EXECUTABLE.to_owned(),
        "-p".into(),
        PROFILE.into(),
        "-D".into(),
        format!("WORKSPACE={root_text}"),
        "-D".into(),
        format!("PRIVATE_TMP={temp_text}"),
        "-D".into(),
        format!("WRITABLE={writable}"),
        "--".into(),
    ];
    arguments.extend_from_slice(argv);
    let environment = [
        "PATH=/usr/bin:/bin".to_owned(),
        format!("HOME={temp_text}"),
        format!("TMPDIR={temp_text}"),
        "LANG=C.UTF-8".to_owned(),
    ];
    let mut descriptors: Vec<libc::proc_fdinfo> =
        (0..65_536).map(|_| unsafe { std::mem::zeroed() }).collect();
    // Resolve the libproc symbol before fork and reject an unavailable query.
    let descriptor_bytes = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDLISTFDS,
            0,
            descriptors.as_mut_ptr().cast(),
            std::mem::size_of_val(descriptors.as_slice()) as libc::c_int,
        )
    };
    if descriptor_bytes <= 0
        || descriptor_bytes as usize >= std::mem::size_of_val(descriptors.as_slice())
    {
        return Err(WorkerError::new(
            "sandbox_requirement_unavailable",
            "Cannot enumerate all inherited host descriptors",
        ));
    }
    let mut child_signal: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut child_signal) } != 0
        || child_signal.sa_sigaction == libc::SIG_IGN
        || child_signal.sa_flags & libc::SA_NOCLDWAIT != 0
    {
        return Err(WorkerError::new(
            "sandbox_requirement_unavailable",
            "The embedding host must permit waiting for child processes",
        ));
    }
    let mut data = SpawnData {
        executable: CString::new(EXECUTABLE).unwrap(),
        arguments: arguments
            .into_iter()
            .map(|s| CString::new(s).unwrap())
            .collect(),
        environment: environment
            .into_iter()
            .map(|s| CString::new(s).unwrap())
            .collect(),
        root_path: cstring(&root_path)?,
        cwd_path: cstring(&cwd_path)?,
        root_identity: Identity::of(&workspace.root)?,
        cwd_identity: Identity::of(&directory.file)?,
        cwd_fd: directory.file.as_raw_fd(),
        limits: sandbox.limits.clone(),
        descriptors,
    };
    let argument_ptrs: Vec<_> = data
        .arguments
        .iter()
        .map(|s| s.as_ptr())
        .chain([std::ptr::null()])
        .collect();
    let environment_ptrs: Vec<_> = data
        .environment
        .iter()
        .map(|s| s.as_ptr())
        .chain([std::ptr::null()])
        .collect();
    let (input_read, input_write) = pipe()?;
    let (out_read, out_write) = pipe()?;
    let (err_read, err_write) = pipe()?;
    let (control_read, control_write) = pipe()?;
    let (report_read, report_write) = pipe()?;
    let (setup_read, setup_write) = pipe()?;
    directory.validate()?;
    let host_pid = unsafe { libc::getpid() };
    resolve_mach_symbols();
    // SAFETY: the child enters a libc-only, never-returning supervisor. Its
    // argv/env/profile buffers remain valid in its private fork snapshot.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if pid == 0 {
        unsafe {
            supervisor(
                &mut data,
                &argument_ptrs,
                &environment_ptrs,
                host_pid,
                input_read.as_raw_fd(),
                out_write.as_raw_fd(),
                err_write.as_raw_fd(),
                control_read.as_raw_fd(),
                report_write.as_raw_fd(),
                setup_read.as_raw_fd(),
                setup_write.as_raw_fd(),
            );
        }
    }
    drop((argument_ptrs, environment_ptrs, data));
    let mut guard = Supervised {
        pid,
        control: Some(control_write),
        waited: false,
        private: Some(private),
    };
    drop((
        input_read,
        out_write,
        err_write,
        control_read,
        report_write,
        setup_read,
        setup_write,
    ));
    let input = async {
        let mut writer = tokio::fs::File::from_std(input_write);
        if let Err(error) = writer.write_all(stdin.unwrap_or_default().as_bytes()).await
            && error.kind() != io::ErrorKind::BrokenPipe
        {
            return Err(WorkerError::new(
                "process_io",
                "Failed to deliver standard input",
            ));
        }
        writer.shutdown().await?;
        Ok::<_, WorkerError>(())
    };
    let out_task = tokio::spawn(drain_live(
        tokio::fs::File::from_std(out_read),
        sandbox.limits.output_bytes,
        live.clone(),
        false,
    ));
    let err_task = tokio::spawn(drain_live(
        tokio::fs::File::from_std(err_read),
        sandbox.limits.output_bytes,
        live,
        true,
    ));
    let output = async {
        let mut report = [0_u8; 5];
        let mut reader = tokio::fs::File::from_std(report_read);
        tokio::select! {
            result = reader.read_exact(&mut report) => { result?; }
            _ = cancel.cancelled() => {
                guard.stop();
                reader.read_exact(&mut report).await?;
            }
        }
        Ok::<_, WorkerError>(report)
    };
    let (input_result, report) = tokio::join!(input, output);
    guard.stop();
    let reap = guard.reap().await;
    let out = out_task
        .await
        .map_err(|_| WorkerError::new("process_io", "Output worker failed"))?;
    let err = err_task
        .await
        .map_err(|_| WorkerError::new("process_io", "Output worker failed"))?;
    reap?;
    let report = report.map_err(|_| {
        WorkerError::new(
            "sandbox_unavailable",
            "Native supervisor failed before reporting a completed invocation",
        )
    })?;
    if report[0] == 3 {
        return Err(WorkerError::new(
            "workspace_conflict",
            "Workspace or working-directory identity changed during execution",
        ));
    }
    if report[0] == 4 {
        return Err(WorkerError::new(
            "cleanup_failed",
            "Native supervisor could not verify process cleanup",
        ));
    }
    let status = i32::from_ne_bytes(report[1..].try_into().unwrap());
    if report[0] == 5 {
        return Err(WorkerError::new(
            "sandbox_requirement_unavailable",
            "Native launcher could not establish its required descriptors or resource limits",
        ));
    }
    if (7..=23).contains(&report[0]) {
        return Err(MachSetupError {
            stage: report[0],
            detail: status,
        }
        .into_worker_error());
    }
    input_result?;
    let (stdout, stdout_truncated) = out?;
    let (stderr, stderr_truncated) = err?;
    directory.validate()?;
    workspace.revalidate().await?;
    Ok(ProcessOutput {
        exit_code: libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status)),
        stdout: String::from_utf8_lossy(&stdout).into(),
        stderr: String::from_utf8_lossy(&stderr).into(),
        truncated: stdout_truncated || stderr_truncated,
        cancelled: report[0] == 1,
        timed_out: report[0] == 2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seatbelt_profile_starts_native_payload_before_ipc_probes() {
        use std::{
            os::unix::process::ExitStatusExt,
            process::{Command, Stdio},
            time::{Duration, Instant},
        };

        // Keep path aliases and payload Mach calls out of this startup test.
        // The fixed host-built payload only prints a marker and exits.
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let workspace = root.join("workspace with spaces");
        let private = root.join("private temp");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private).unwrap();
        let source = root.join("entry.rs");
        let executable = workspace.join("native entry");
        std::fs::write(
            &source,
            "fn main() { println!(\"native-profile-started\"); }\n",
        )
        .unwrap();
        let compiled = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition=2024", "-C", "debuginfo=0"])
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "native startup fixture compilation: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let control = Command::new(&executable).output().unwrap();
        assert!(control.status.success(), "outside control: {control:?}");
        assert_eq!(control.stdout, b"native-profile-started\n");

        let run_profile = |profile: &str| {
            let mut child = Command::new(EXECUTABLE)
                .args(["-p", profile, "-D"])
                .arg(format!("WORKSPACE={}", workspace.display()))
                .arg("-D")
                .arg(format!("PRIVATE_TMP={}", private.display()))
                .args(["-D", "WRITABLE=false", "--"])
                .arg(&executable)
                .current_dir(&workspace)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &private)
                .env("TMPDIR", &private)
                .env("LANG", "C.UTF-8")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut timed_out = false;
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    timed_out = true;
                    child.kill().unwrap();
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            (output, timed_out)
        };
        let (output, timed_out) = run_profile(PROFILE);
        if !timed_out && output.status.success() && output.stdout == b"native-profile-started\n" {
            return;
        }

        // Failure-only differential controls, never a production fallback or a
        // passing substitute for PROFILE. No input or executable is user supplied.
        let mut diagnostics = format!(
            "production: status={}, signal={:?}, timeout={timed_out}, stdout={:?}, stderr={:?}",
            output.status,
            output.status.signal(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for (name, getter, setter) in [
            ("without explicit getter denial", true, false),
            ("without explicit setter denial", false, true),
            ("without both explicit denials", true, true),
        ] {
            let mut profile = PROFILE.to_owned();
            if getter {
                profile = profile.replace("(deny mach-task-special-port-get)\n", "");
            }
            if setter {
                profile = profile.replace("(deny mach-task-special-port-set)\n", "");
            }
            let (output, timed_out) = run_profile(&profile);
            diagnostics.push_str(&format!(
                "\n{name}: status={}, signal={:?}, timeout={timed_out}, stdout={:?}, stderr={:?}",
                output.status,
                output.status.signal(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        panic!("production Seatbelt profile did not reach the native entry point\n{diagnostics}");
    }

    fn check_scrub_in_fork() {
        use std::io::Read;
        let (mut reader, writer) = pipe().unwrap();
        resolve_mach_symbols();
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "cannot fork Mach cleanup fixture");
        if child == 0 {
            // Exercise the real fork snapshot and backend routine. No host
            // test-harness ports are changed and no Rust cleanup runs here.
            let result = unsafe { scrub_inherited_mach() };
            let mut report = [0_u8; 5];
            if let Err(error) = result {
                report[0] = error.stage;
                report[1..].copy_from_slice(&error.detail.to_ne_bytes());
            }
            unsafe {
                raw_write(writer.as_raw_fd(), &report);
                libc::_exit(0);
            }
        }
        drop(writer);
        let mut report = [0_u8; 5];
        let received = reader.read_exact(&mut report);
        let mut status = 0;
        loop {
            let waited = unsafe { libc::waitpid(child, &mut status, 0) };
            if waited < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            assert_eq!(waited, child);
            break;
        }
        assert_eq!(status, 0, "Mach cleanup fixture died: wait status={status}");
        received.unwrap();
        if report[0] != 0 {
            panic!(
                "{}",
                MachSetupError {
                    stage: report[0],
                    detail: i32::from_ne_bytes(report[1..].try_into().unwrap())
                }
                .into_worker_error()
            );
        }
    }

    #[test]
    fn inherited_mach_ports_are_cleared_in_fork_snapshot() {
        check_scrub_in_fork();
    }

    #[test]
    fn actual_kernel_host_port_is_accepted() {
        #[allow(deprecated)]
        let (task, host) = unsafe { (libc::mach_task_self(), libc::mach_host_self()) };
        let mut selected = 0;
        assert_eq!(
            unsafe { task_get_special_port(task, TASK_HOST_PORT, &mut selected) },
            0
        );
        // The independent host-self trap must return the same right. A wrong
        // special-port selector must never be hidden by loosening the type check.
        assert_ne!(host, 0);
        assert_eq!(selected, host);
        unsafe { inspect_host_port(task, host) }.unwrap();
        assert_eq!(unsafe { mach_port_deallocate(task, selected) }, 0);
        assert_eq!(unsafe { mach_port_deallocate(task, host) }, 0);
        unsafe { verify_host_port() }.unwrap();
    }

    #[test]
    fn special_port_selectors_match_native_sdk() {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };
        // Compile against the runner's real SDK, not a second handwritten
        // table. This test needs no linking or privileges and does not run any
        // untrusted code. A selector/type mix-up fails at compile time here.
        let source = format!(
            "#include <mach/task_special_ports.h>\n\
             #include <mach/kern_return.h>\n\
             #include <mach/exception_types.h>\n\
             _Static_assert(TASK_HOST_PORT == {TASK_HOST_PORT}, \"TASK_HOST_PORT\");\n\
             _Static_assert(TASK_BOOTSTRAP_PORT == {TASK_BOOTSTRAP_PORT}, \"TASK_BOOTSTRAP_PORT\");\n\
             _Static_assert(TASK_ACCESS_PORT == {TASK_ACCESS_PORT}, \"TASK_ACCESS_PORT\");\n\
             _Static_assert((EXC_MASK_ALL | EXC_MASK_CRASH | EXC_MASK_CORPSE_NOTIFY) == {EXCEPTION_MASK}, \"EXCEPTION_MASK\");\n\
             _Static_assert(KERN_DENIED == {KERN_DENIED}, \"KERN_DENIED\");\n"
        );
        let mut compiler = Command::new("/usr/bin/xcrun")
            .args([
                "--sdk",
                "macosx",
                "clang",
                "-std=c11",
                "-Werror",
                "-fsyntax-only",
                "-x",
                "c",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("native SDK compiler must be available");
        compiler
            .stdin
            .take()
            .unwrap()
            .write_all(source.as_bytes())
            .unwrap();
        let output = compiler.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "Native Mach ABI mismatch: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn task_and_missing_ports_are_not_host_ports() {
        #[allow(deprecated)]
        let task = unsafe { libc::mach_task_self() };
        let error = unsafe { inspect_host_port(task, task) }.unwrap_err();
        assert_eq!(error.stage, 9);
        assert_eq!(error.detail, 2); // IKOT_TASK_CONTROL
        assert_eq!(unsafe { inspect_host_port(task, 0) }.unwrap_err().stage, 8);
    }
}
