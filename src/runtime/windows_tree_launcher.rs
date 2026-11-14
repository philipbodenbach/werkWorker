//! Trusted host-account bootstrap for the dedicated-account runner.
//! CreateProcessWithLogonW rejects extended startup attributes on the supported
//! target. This bootstrap is assigned atomically to the host's Job first; its
//! suspended logon child inherits that Job and is handed back by retained handles.
use super::windows_provision;
use crate::core::{Result, WorkerError};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::File,
    io::Write,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{JobObjects::*, Pipes::*, Threading::*},
};

const MAX_REQUEST: usize = 65536;
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    pub version: u32,
    pub provisioning: PathBuf,
    pub manifest_digest: String,
    pub workspace: PathBuf,
    pub pipe_prefix: String,
    pub environment: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "status", deny_unknown_fields)]
pub(super) enum Reply {
    Ready {
        version: u32,
        pid: u32,
        process_handle: u64,
        thread_handle: u64,
    },
    Failed {
        version: u32,
        message: String,
    },
}
fn invalid(message: impl Into<String>) -> WorkerError {
    WorkerError::new("sandbox_unavailable", message)
}
fn failure(operation: &str) -> WorkerError {
    invalid(format!("{operation}: {}", std::io::Error::last_os_error()))
}
fn wide(value: impl AsRef<OsStr>) -> Result<Vec<u16>> {
    let mut value: Vec<_> = value.as_ref().encode_wide().collect();
    if value.contains(&0) {
        return Err(invalid("NUL in launcher input"));
    }
    value.push(0);
    Ok(value)
}
fn raw(value: &OwnedHandle) -> HANDLE {
    value.as_raw_handle()
}
unsafe fn own(value: HANDLE) -> Result<OwnedHandle> {
    if value.is_null() || value == INVALID_HANDLE_VALUE {
        return Err(failure("Create launcher handle"));
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(value) })
}
fn quote(value: &OsStr) -> Vec<u16> {
    let mut output = vec![34];
    let mut slashes = 0;
    for value in value.encode_wide() {
        if value == 92 {
            slashes += 1;
            continue;
        }
        output.extend(std::iter::repeat_n(
            92,
            if value == 34 {
                slashes * 2 + 1
            } else {
                slashes
            },
        ));
        slashes = 0;
        output.push(value);
    }
    output.extend(std::iter::repeat_n(92, slashes * 2));
    output.push(34);
    output
}
fn pipe(prefix: &str, suffix: &str, access: u32, host: u32) -> Result<OwnedHandle> {
    let path = wide(format!("{prefix}.{suffix}"))?;
    let handle = unsafe {
        own(CreateFileW(
            path.as_ptr(),
            access,
            0,
            null(),
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            null_mut(),
        ))?
    };
    let mut actual = 0;
    if unsafe { GetNamedPipeServerProcessId(raw(&handle), &mut actual) } == 0 || actual != host {
        return Err(invalid("Launcher pipe server differs from pinned host"));
    }
    Ok(handle)
}
fn read_bounded(handle: &OwnedHandle, host: &OwnedHandle, limit: usize) -> Result<Vec<u8>> {
    let start = Instant::now();
    let mut bytes = Vec::new();
    loop {
        if unsafe { WaitForSingleObject(raw(host), 0) } != WAIT_TIMEOUT {
            return Err(invalid("Launcher host ended before handoff"));
        }
        if start.elapsed() >= HANDOFF_TIMEOUT {
            return Err(invalid("Launcher handoff timed out"));
        }
        let mut available = 0;
        if unsafe {
            PeekNamedPipe(
                raw(handle),
                null_mut(),
                0,
                null_mut(),
                &mut available,
                null_mut(),
            )
        } == 0
        {
            return match unsafe { GetLastError() } {
                ERROR_BROKEN_PIPE => Ok(bytes),
                _ => Err(failure("Inspect launcher request")),
            };
        }
        if available == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let mut chunk = [0u8; 4096];
        let mut read = 0;
        if unsafe {
            ReadFile(
                raw(handle),
                chunk.as_mut_ptr(),
                available.min(chunk.len() as u32),
                &mut read,
                null_mut(),
            )
        } == 0
        {
            return Err(failure("Read launcher request"));
        }
        if bytes.len() + read as usize > limit {
            return Err(invalid("Launcher transport exceeded bound"));
        }
        bytes.extend_from_slice(&chunk[..read as usize]);
    }
}
fn contained() -> Result<()> {
    let mut member = 0;
    if unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &mut member) } == 0 || member == 0 {
        return Err(invalid(
            "Trusted launcher was not assigned to its Job before execution",
        ));
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    if unsafe {
        QueryInformationJobObject(
            null_mut(),
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            null_mut(),
        )
    } == 0
    {
        return Err(failure("Read trusted launcher Job limits"));
    }
    let required = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_JOB_TIME;
    if info.BasicLimitInformation.LimitFlags & required != required
        || info.BasicLimitInformation.LimitFlags
            & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK)
            != 0
        || info.BasicLimitInformation.ActiveProcessLimit < 2
        || info.BasicLimitInformation.PerJobUserTimeLimit <= 0
        || info.JobMemoryLimit == 0
    {
        return Err(invalid(
            "Trusted launcher Job lacks finite non-breakaway limits",
        ));
    }
    Ok(())
}
struct Child {
    process: OwnedHandle,
    thread: OwnedHandle,
    released: bool,
}
impl Drop for Child {
    fn drop(&mut self) {
        if !self.released {
            unsafe {
                TerminateProcess(raw(&self.process), 125);
                // The host's supervisor owns the Job and lease independently.
                WaitForSingleObject(raw(&self.process), INFINITE);
            }
        }
    }
}
fn launch(
    request: Request,
    host: &OwnedHandle,
    host_pid: u32,
    reply: &mut File,
    ack: &OwnedHandle,
) -> Result<()> {
    if request.version != 1 {
        return Err(invalid("Unsupported launcher request version"));
    }
    let suffix = request
        .pipe_prefix
        .strip_prefix(r"\\.\pipe\werk-tree-")
        .filter(|s| s.len() == 36 && uuid::Uuid::parse_str(s).is_ok())
        .ok_or_else(|| invalid("Invalid runner pipe namespace"))?;
    let _ = suffix;
    let provision = windows_provision::load(&request.provisioning, &request.workspace)?;
    if provision.manifest_digest != request.manifest_digest {
        return Err(invalid(
            "Launcher provisioning differs from host-pinned receipt",
        ));
    }
    let image = wide(&provision.manifest.runner)?;
    let mut command = quote(provision.manifest.runner.as_os_str());
    for argument in [
        "--pipe-prefix".to_owned(),
        request.pipe_prefix,
        "--host-pid".to_owned(),
        host_pid.to_string(),
    ] {
        command.push(32);
        command.extend(quote(OsStr::new(&argument)));
    }
    command.push(0);
    if command.len() > 1024 {
        return Err(invalid("Runner command exceeds logon API limit"));
    }
    let username = wide(&provision.manifest.username)?;
    let domain = wide(".")?;
    let cwd = wide(&request.workspace)?;
    let mut environment = Vec::new();
    for (name, value) in &request.environment {
        if name.is_empty() || name.contains('=') {
            return Err(invalid("Invalid launcher environment name"));
        }
        environment.extend(wide(format!("{name}={value}"))?);
    }
    environment.push(0);
    if environment.len() == 1 {
        environment.push(0);
    }
    if environment.len() > 32767 {
        return Err(invalid("Launcher environment too large"));
    }
    let password = provision.password()?;
    let mut startup: STARTUPINFOW = unsafe { zeroed() };
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    if unsafe {
        CreateProcessWithLogonW(
            username.as_ptr(),
            domain.as_ptr(),
            password.as_ptr(),
            0,
            image.as_ptr(),
            command.as_mut_ptr(),
            CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &startup,
            &mut info,
        )
    } == 0
    {
        return Err(failure("Create suspended dedicated-account runner"));
    }
    drop(password);
    let mut child = Child {
        process: unsafe { own(info.hProcess)? },
        thread: unsafe { own(info.hThread)? },
        released: false,
    };
    // The authenticated host pulls copies through its retained launcher handle.
    // Keeping the originals alive until ACK avoids both PID races and leaked
    // remote handles if either duplication or transport fails.
    let ready = Reply::Ready {
        version: 1,
        pid: info.dwProcessId,
        process_handle: raw(&child.process) as usize as u64,
        thread_handle: raw(&child.thread) as usize as u64,
    };
    let mut bytes = serde_json::to_vec(&ready)?;
    bytes.push(b'\n');
    reply.write_all(&bytes)?;
    reply.flush()?;
    if read_bounded(ack, host, 1)? != [1] {
        return Err(invalid(
            "Host did not acknowledge verified suspended runner",
        ));
    }
    // Only the host resumes this thread, after this launcher has exited.
    child.released = true;
    Ok(())
}
fn execute() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 7
        || args[0] != "--host-launcher"
        || args[1] != "--pipe-prefix"
        || args[3] != "--host-pid"
        || args[5] != "--host-handle"
    {
        return Err(invalid("Invalid trusted launcher arguments"));
    }
    let prefix = args[2]
        .to_str()
        .ok_or_else(|| invalid("Invalid launcher pipe name"))?;
    let suffix = prefix
        .strip_prefix(r"\\.\pipe\werk-launch-")
        .ok_or_else(|| invalid("Invalid launcher pipe namespace"))?;
    if suffix.len() != 36 || uuid::Uuid::parse_str(suffix).is_err() {
        return Err(invalid("Invalid launcher pipe UUID"));
    }
    let pid = args[4]
        .to_str()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|p| *p != 0)
        .ok_or_else(|| invalid("Invalid launcher host PID"))?;
    let handle = args[6]
        .to_str()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|h| *h != 0 && *h != usize::MAX)
        .ok_or_else(|| invalid("Invalid pinned host handle"))?;
    let host = unsafe { own(handle as HANDLE)? };
    if unsafe { GetProcessId(raw(&host)) } != pid
        || unsafe { WaitForSingleObject(raw(&host), 0) } != WAIT_TIMEOUT
    {
        return Err(invalid(
            "Inherited host handle does not identify the live trusted host",
        ));
    }
    contained()?;
    let mut reply = File::from(pipe(prefix, "reply", GENERIC_WRITE, pid)?);
    let result = (|| {
        let input = pipe(prefix, "request", GENERIC_READ, pid)?;
        let ack = pipe(prefix, "ack", GENERIC_READ, pid)?;
        let request: Request = serde_json::from_slice(&read_bounded(&input, &host, MAX_REQUEST)?)?;
        launch(request, &host, pid, &mut reply, &ack)
    })();
    if let Err(error) = &result {
        let response = Reply::Failed {
            version: 1,
            message: error.to_string(),
        };
        if let Ok(mut bytes) = serde_json::to_vec(&response) {
            bytes.push(b'\n');
            let _ = reply.write_all(&bytes);
        }
    }
    result
}
pub(super) fn main() -> i32 {
    match execute() {
        Ok(()) => 0,
        Err(_) => 125,
    }
}
