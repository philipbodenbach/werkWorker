//! Host-only retained process objects for the complete lifetime of an owned Job.
//! IOCP notifications only discover candidates. They never prove termination:
//! cleanup requires an exact lifetime accounting count and signaled handles.
use crate::core::{Result, WorkerError};
use std::{
    collections::{HashMap, HashSet},
    mem::{size_of, zeroed},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::null_mut,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};
use windows_sys::Win32::{
    Foundation::*,
    System::{
        IO::{CreateIoCompletionPort, GetQueuedCompletionStatus, PostQueuedCompletionStatus},
        JobObjects::*,
        SystemServices::{JOB_OBJECT_MSG_NEW_PROCESS, JOB_OBJECT_QUERY},
        Threading::{
            GetCurrentProcess, GetProcessId, GetProcessTimes, OpenProcess,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        },
    },
};

pub(super) const MAX_TRACKED_PROCESSES: usize = 4096;
const PROCESS_ACCESS: u32 = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
const JOB_KEY: usize = 1;
const STOP_KEY: usize = usize::MAX;
// A shutdown fallback for a failed wake packet; this is not a cleanup grace period.
const PORT_WAIT_MS: u32 = 100;

type Identity = (u32, u64);
fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
}
fn invalid(message: impl Into<String>) -> WorkerError {
    WorkerError::new("cleanup_failed", message)
}
fn native_error(operation: &str) -> WorkerError {
    invalid(format!("{operation}: {}", std::io::Error::last_os_error()))
}
fn duplicate(handle: HANDLE, access: u32) -> Result<OwnedHandle> {
    let mut copy = null_mut();
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &mut copy,
            access,
            0,
            0,
        )
    } == 0
    {
        return Err(native_error("Duplicate host process-tracker handle"));
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(copy) })
}
fn accounting(job: HANDLE) -> Result<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION> {
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
    if unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            null_mut(),
        )
    } == 0
    {
        return Err(native_error("Query complete Job lifetime accounting"));
    }
    Ok(info)
}
fn identity(handle: HANDLE, job: HANDLE, expected_pid: Option<u32>) -> Result<Identity> {
    let pid = unsafe { GetProcessId(handle) };
    if pid == 0 {
        return Err(native_error("Read retained Job process ID"));
    }
    if expected_pid.is_some_and(|expected| expected != pid) {
        return Err(invalid(
            "Job notification PID differs from its opened process",
        ));
    }
    let mut member = 0;
    if unsafe { IsProcessInJob(handle, job, &mut member) } == 0 {
        return Err(native_error("Verify exact tracked Job membership"));
    }
    if member == 0 {
        return Err(invalid(format!(
            "Process {pid} is outside the exact tracked Job"
        )));
    }
    let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
        unsafe { zeroed() };
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(native_error("Read retained Job process creation time"));
    }
    Ok((
        pid,
        (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
    ))
}

struct State {
    members: HashMap<Identity, OwnedHandle>,
    known_pids: HashSet<u32>,
    error: Option<WorkerError>,
    monitor_running: bool,
    last_total: u32,
    last_active: u32,
}
impl State {
    fn fail(&mut self, error: WorkerError) -> WorkerError {
        self.error.get_or_insert(error).clone()
    }
    fn retain(
        &mut self,
        handle: OwnedHandle,
        job: HANDLE,
        expected_pid: Option<u32>,
    ) -> Result<()> {
        let id = identity(raw(&handle), job, expected_pid)?;
        if let Some(existing) = self.members.get(&id) {
            // PID/birth are diagnostic identity, not a replacement for kernel
            // object identity. Two different objects may never share one credit.
            if unsafe { CompareObjectHandles(raw(existing), raw(&handle)) } == 0 {
                return Err(invalid(format!(
                    "Distinct process objects share PID/birth {id:?}"
                )));
            }
            return Ok(());
        }
        if self.members.len() >= MAX_TRACKED_PROCESSES {
            return Err(invalid(format!(
                "Job lifetime exceeds {MAX_TRACKED_PROCESSES} retained process objects"
            )));
        }
        self.known_pids.insert(id.0);
        self.members.insert(id, handle);
        Ok(())
    }
}
struct Shared {
    // Both handles stay exclusively in the host. The monitor's Arc keeps the Job
    // alive even if its owner's original handle is dropped before tracker Drop.
    job: OwnedHandle,
    port: OwnedHandle,
    state: Mutex<State>,
    stopping: AtomicBool,
}
impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            state.fail(invalid("Job member tracker state was poisoned"));
            state
        })
    }
    fn fail(&self, error: WorkerError) {
        self.lock().fail(error);
    }
    fn pin_notification(&self, pid: u32) -> Result<()> {
        let mut state = self.lock();
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        // Repeated notifications for a retained PID cannot add a member. If a
        // distinct object ever reused it, the authoritative lifetime count would
        // remain larger until an independently verified handle was registered.
        if state.known_pids.contains(&pid) {
            return Ok(());
        }
        let handle = unsafe { OpenProcess(PROCESS_ACCESS, 0, pid) };
        if handle.is_null() {
            return Err(native_error(&format!("Open notified Job process {pid}")));
        }
        let owned = unsafe { OwnedHandle::from_raw_handle(handle) };
        state.retain(owned, raw(&self.job), Some(pid))
    }
}
fn monitor(shared: &Shared) {
    while !shared.stopping.load(Ordering::Acquire) {
        let mut message = 0;
        let mut key = 0;
        let mut value = null_mut();
        if unsafe {
            GetQueuedCompletionStatus(
                raw(&shared.port),
                &mut message,
                &mut key,
                &mut value,
                PORT_WAIT_MS,
            )
        } == 0
        {
            let error = unsafe { GetLastError() };
            if error == WAIT_TIMEOUT && value.is_null() {
                continue;
            }
            shared.fail(invalid(format!(
                "Job completion-port receive failed: Windows error {error}"
            )));
            return;
        }
        if key == STOP_KEY && shared.stopping.load(Ordering::Acquire) {
            return;
        }
        if key != JOB_KEY {
            shared.fail(invalid("Unexpected Job completion-port key"));
            return;
        }
        if message == JOB_OBJECT_MSG_NEW_PROCESS {
            let pid = match u32::try_from(value as usize) {
                Ok(pid) if pid != 0 => pid,
                _ => {
                    shared.fail(invalid("Invalid NEW_PROCESS notification PID"));
                    return;
                }
            };
            if let Err(error) = shared.pin_notification(pid) {
                shared.fail(error);
                return;
            }
        }
        // EXIT, ACTIVE_PROCESS_ZERO and limit messages are merely hints too.
        // They never remove a handle or reduce the required lifetime count.
    }
}

pub(super) struct MemberTracker {
    shared: Arc<Shared>,
    monitor: Option<JoinHandle<()>>,
}
impl MemberTracker {
    /// Must be called on a fresh empty Job before its first assignment, including
    /// atomic PROC_THREAD_ATTRIBUTE_JOB_LIST assignment during CreateProcess.
    pub(super) fn new(job: &OwnedHandle) -> Result<Self> {
        let info = accounting(raw(job))?;
        if info.TotalProcesses != 0 || info.ActiveProcesses != 0 {
            return Err(invalid(
                "Job member tracking must begin before first assignment",
            ));
        }
        let retained_job = duplicate(raw(job), JOB_OBJECT_QUERY)?;
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1) };
        if port.is_null() {
            return Err(native_error("Create host Job completion port"));
        }
        let port = unsafe { OwnedHandle::from_raw_handle(port) };
        let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
            CompletionKey: JOB_KEY as *mut _,
            CompletionPort: raw(&port),
        };
        if unsafe {
            SetInformationJobObject(
                raw(job),
                JobObjectAssociateCompletionPortInformation,
                (&association as *const JOBOBJECT_ASSOCIATE_COMPLETION_PORT).cast(),
                size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
            )
        } == 0
        {
            return Err(native_error(
                "Associate Job completion port before first assignment",
            ));
        }
        let shared = Arc::new(Shared {
            job: retained_job,
            port,
            state: Mutex::new(State {
                members: HashMap::new(),
                known_pids: HashSet::new(),
                error: None,
                monitor_running: true,
                last_total: 0,
                last_active: 0,
            }),
            stopping: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let monitor = std::thread::Builder::new()
            .name("werk-job-members".into())
            .spawn(move || {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| monitor(&worker)));
                let mut state = worker.lock();
                state.monitor_running = false;
                if result.is_err() {
                    state.fail(invalid("Job member monitor panicked"));
                } else if !worker.stopping.load(Ordering::Acquire) && state.error.is_none() {
                    state.fail(invalid("Job member monitor stopped unexpectedly"));
                }
            })
            .map_err(|error| invalid(format!("Start host Job member monitor: {error}")))?;
        Ok(Self {
            shared,
            monitor: Some(monitor),
        })
    }
    /// Registers an already retained launcher/runner/payload handle before its
    /// initial ResumeThread. This takes a noninheritable, least-access duplicate.
    pub(super) fn register_known(&self, handle: HANDLE) -> Result<()> {
        let mut state = self.shared.lock();
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        let result = duplicate(handle, PROCESS_ACCESS)
            .and_then(|handle| state.retain(handle, raw(&self.shared.job), None));
        result.map_err(|error| state.fail(error))
    }
    pub(super) fn failure(&self) -> Option<WorkerError> {
        self.shared.lock().error.clone()
    }
    pub(super) fn diagnostics(&self) -> String {
        let state = self.shared.lock();
        format!(
            "retained_members={}, total_processes={}, active_processes={}, monitor_running={}, failure={}",
            state.members.len(),
            state.last_total,
            state.last_active,
            state.monitor_running,
            state
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("none")
        )
    }
    /// Wait samples precede authoritative accounting while the ledger is locked.
    /// Thus a spawn between the waits and accounting cannot hide behind an older
    /// count. A missing notification remains a count gap, never an exited credit.
    pub(super) fn confirmed_drain(&self) -> Result<bool> {
        let mut state = self.shared.lock();
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        if !state.monitor_running {
            return Err(state.fail(invalid("Job member monitor is not running")));
        }
        let mut all_signaled = true;
        for (id, handle) in &state.members {
            match unsafe { WaitForSingleObject(raw(handle), 0) } {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => all_signaled = false,
                status => {
                    let error = invalid(format!(
                        "Wait retained Job process {id:?} returned {status}: {}",
                        std::io::Error::last_os_error()
                    ));
                    return Err(state.fail(error));
                }
            }
        }
        let info = match accounting(raw(&self.shared.job)) {
            Ok(info) => info,
            Err(error) => return Err(state.fail(error)),
        };
        state.last_total = info.TotalProcesses;
        state.last_active = info.ActiveProcesses;
        if info.TotalProcesses as usize > MAX_TRACKED_PROCESSES {
            return Err(state.fail(invalid(format!(
                "Job lifetime accounting exceeds {MAX_TRACKED_PROCESSES} retained processes"
            ))));
        }
        if (info.TotalProcesses as usize) < state.members.len()
            || info.ActiveProcesses > info.TotalProcesses
        {
            return Err(state.fail(invalid(
                "Job lifetime accounting is below the verified process-object census",
            )));
        }
        Ok(all_signaled
            && info.ActiveProcesses == 0
            && info.TotalProcesses as usize == state.members.len())
    }
}
impl Drop for MemberTracker {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        // The finite receive timeout also ends the thread if posting fails.
        unsafe {
            PostQueuedCompletionStatus(raw(&self.shared.port), 0, STOP_KEY, null_mut());
        }
        if let Some(thread) = self.monitor.take()
            && thread.join().is_err()
        {
            self.shared.fail(invalid("Join Job member monitor failed"));
        }
        // Only now may the private port, retained Job and process handles drop.
    }
}
