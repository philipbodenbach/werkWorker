//! Windows LPAC execution. Provisioning is an explicit host operation; execution
//! never changes ACLs on an existing workspace or copies its contents elsewhere.
use super::{LiveOutput, ProcessLimits, ProcessOutput, Sandbox, Workspace, drain_live};
use crate::{core::*, policy::relative_path};
use serde_json::{Value, json};
use std::{
    ffi::{OsStr, OsString, c_void},
    fs::File,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::Arc,
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, Isolation::*, *},
    Storage::FileSystem::*,
    System::{
        Com::CoTaskMemFree, JobObjects::*, LibraryLoader::*, Pipes::CreatePipe,
        SystemInformation::*, Threading::*,
    },
    UI::Shell::FOLDERID_LocalAppData,
};

const POLICY_VERSION: u32 = 1;
const OPT_OUT_ALL_APPLICATION_PACKAGES: u32 = 1;
// Documented UpdateProcThreadAttribute creation mitigation (1 << 28).
const WIN32K_SYSTEM_CALL_DISABLE: u64 = 1 << 28;
const SECURITY_INFO: u32 = DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;

fn error(operation: &str) -> WorkerError {
    WorkerError::new(
        "sandbox_unavailable",
        format!(
            "{operation}: Windows error {}",
            std::io::Error::last_os_error()
        ),
    )
}
fn wide(value: impl AsRef<OsStr>) -> Result<Vec<u16>> {
    let mut value: Vec<u16> = value.as_ref().encode_wide().collect();
    if value.contains(&0) {
        return Err(WorkerError::new("path_denied", "NUL in Windows argument"));
    }
    value.push(0);
    Ok(value)
}
unsafe fn take_handle(handle: HANDLE) -> Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(error("Create native handle"));
    }
    // SAFETY: caller transfers a newly owned non-pseudo handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}
fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
}
struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
struct Sid(PSID);
impl Drop for Sid {
    fn drop(&mut self) {
        unsafe {
            FreeSid(self.0);
        }
    }
}
impl Sid {
    fn derive(name: &str) -> Result<Self> {
        let name = wide(name)?;
        let mut sid = null_mut();
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
        if hr < 0 {
            return Err(WorkerError::new(
                "sandbox_unavailable",
                format!("Derive AppContainer SID failed: {hr:#x}"),
            ));
        }
        Ok(Self(sid))
    }
    fn text(&self) -> Result<String> {
        sid_text(self.0)
    }
}
fn sid_text(sid: PSID) -> Result<String> {
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(error("Read security identity"));
    }
    let _allocation = LocalAllocation(text.cast());
    Ok(unsafe { read_wide(text) }.to_string_lossy().into_owned())
}
unsafe fn read_wide(value: *const u16) -> OsString {
    let mut len = 0;
    while unsafe { *value.add(len) } != 0 {
        len += 1;
    }
    OsString::from_wide(unsafe { std::slice::from_raw_parts(value, len) })
}
fn user_sid() -> Result<String> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(error("Open host identity"));
    }
    let token = unsafe { take_handle(token)? };
    let mut size = 0;
    unsafe {
        GetTokenInformation(raw(&token), TokenUser, null_mut(), 0, &mut size);
    }
    let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            raw(&token),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(error("Read host identity"));
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    sid_text(user.User.Sid)
}
fn token_information(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<usize>> {
    let mut bytes = 0;
    unsafe { GetTokenInformation(token, class, null_mut(), 0, &mut bytes) };
    if bytes == 0 || bytes > 1024 * 1024 {
        return Err(error("Query suspended token information size"));
    }
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    if unsafe { GetTokenInformation(token, class, buffer.as_mut_ptr().cast(), bytes, &mut bytes) }
        == 0
    {
        return Err(error("Read suspended token information"));
    }
    Ok(buffer)
}
fn grants_probe_access(token: HANDLE, package_sid: &str) -> Result<bool> {
    // The normal user pass accepts Everyone; the lowbox pass requires the
    // package grant. Owner/group are mandatory for the documented AccessCheck API.
    let sddl = wide(format!("O:SYG:SYD:(A;;0x1;;;WD)(A;;0x1;;;{package_sid})"))?;
    let mut descriptor = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(error("Build LPAC access-check descriptor"));
    }
    let descriptor = LocalAllocation(descriptor);
    let mapping = GENERIC_MAPPING {
        GenericRead: 1,
        GenericWrite: 1,
        GenericExecute: 1,
        GenericAll: 1,
    };
    let mut privileges: PRIVILEGE_SET = unsafe { zeroed() };
    let mut bytes = size_of::<PRIVILEGE_SET>() as u32;
    let mut granted = 0;
    let mut allowed = 0;
    if unsafe {
        AccessCheck(
            descriptor.0,
            token,
            1,
            &mapping,
            &mut privileges,
            &mut bytes,
            &mut granted,
            &mut allowed,
        )
    } == 0
    {
        return Err(error("Verify suspended LPAC authorization"));
    }
    Ok(allowed != 0 && granted & 1 != 0)
}
fn verify_token(process: HANDLE, sid: &Sid) -> Result<()> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY | TOKEN_DUPLICATE, &mut token) } == 0 {
        return Err(error("Open suspended child token"));
    }
    let token = unsafe { take_handle(token)? };
    let container = token_information(raw(&token), TokenIsAppContainer)?;
    let package = token_information(raw(&token), TokenAppContainerSid)?;
    let capabilities = token_information(raw(&token), TokenCapabilities)?;
    let container = unsafe { *container.as_ptr().cast::<u32>() };
    let package = unsafe { &*package.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>() };
    // TOKEN_GROUPS is variable length. With zero capabilities the API need
    // only return its count/header, not storage for the first SID_AND_ATTRIBUTES.
    let capability_count = unsafe { *capabilities.as_ptr().cast::<u32>() };
    if container != 1
        || package.TokenAppContainer.is_null()
        || unsafe { EqualSid(package.TokenAppContainer, sid.0) } == 0
        || capability_count != 0
    {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "Suspended child has an unexpected AppContainer identity or capabilities",
        ));
    }
    let mut impersonation = null_mut();
    if unsafe {
        DuplicateTokenEx(
            raw(&token),
            TOKEN_QUERY,
            null(),
            SecurityImpersonation,
            TokenImpersonation,
            &mut impersonation,
        )
    } == 0
    {
        return Err(error("Duplicate token for LPAC access check"));
    }
    let impersonation = unsafe { take_handle(impersonation)? };
    // TokenIsLessPrivilegedAppContainer is rejected from user mode on current
    // Windows releases. Test the actual authorization distinction instead:
    // an explicit package grant succeeds, ALL APPLICATION PACKAGES does not.
    // Both checks finish while the initial process thread is still suspended.
    if !grants_probe_access(raw(&impersonation), &sid.text()?)?
        || grants_probe_access(raw(&impersonation), "AC")?
    {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "Suspended child does not enforce the required LPAC access checks",
        ));
    }
    Ok(())
}
fn profile_name(workspace: &Workspace, writable: bool) -> String {
    format!(
        "werkWorker.v{POLICY_VERSION}.{}.{}",
        &fingerprint(&workspace.identity)[..32],
        if writable { "rw" } else { "ro" }
    )
}
fn profile_path(sid: &Sid) -> Result<PathBuf> {
    let sid = wide(sid.text()?)?;
    let mut path = null_mut();
    let hr = unsafe { GetAppContainerFolderPath(sid.as_ptr(), &mut path) };
    if hr < 0 {
        return Err(WorkerError::new(
            "sandbox_workspace_unprepared",
            "Create a dedicated workspace with sandbox create-workspace first",
        ));
    }
    let result = PathBuf::from(unsafe { read_wide(path) });
    unsafe {
        CoTaskMemFree(path.cast());
    }
    Ok(result)
}
fn local_app_data() -> Result<PathBuf> {
    // Statically importing Shell32 into an SDK consumer also loads its GUI
    // dependencies in a copied payload, incompatible with Win32k lockdown.
    // Resolve this trusted-host-only API from System32 explicitly instead.
    let name = wide("shell32.dll")?;
    let module = unsafe { LoadLibraryExW(name.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module.is_null() {
        return Err(error("Load native known-folder API"));
    }
    struct Module(HMODULE);
    impl Drop for Module {
        fn drop(&mut self) {
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
    let module = Module(module);
    let address = unsafe { GetProcAddress(module.0, c"SHGetKnownFolderPath".as_ptr().cast()) }
        .ok_or_else(|| error("Resolve native known-folder API"))?;
    type GetKnownFolderPath = unsafe extern "system" fn(
        *const windows_sys::core::GUID,
        u32,
        HANDLE,
        *mut *mut u16,
    ) -> i32;
    let get_known_folder: GetKnownFolderPath = unsafe { std::mem::transmute(address) };
    let mut path = null_mut();
    let status = unsafe { get_known_folder(&FOLDERID_LocalAppData, 0, null_mut(), &mut path) };
    if status < 0 {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            format!("Resolve native LocalAppData failed: {status:#x}"),
        ));
    }
    let result = PathBuf::from(unsafe { read_wide(path) });
    unsafe {
        CoTaskMemFree(path.cast());
    }
    Ok(result)
}
fn expected_descriptor(workspace: &Workspace) -> Result<LocalAllocation> {
    let read = Sid::derive(&profile_name(workspace, false))?.text()?;
    let write = Sid::derive(&profile_name(workspace, true))?.text()?;
    // OWNER RIGHTS removes implicit owner WRITE_DAC. The trusted host retains
    // explicit full access; the lowbox pass only grants the selected mode.
    let sddl = wide(format!(
        "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{})(A;OICI;RC;;;OW)(A;OICI;0x1201ff;;;{write})(A;OICI;0x1200a9;;;{read})S:(ML;OICI;NW;;;LW)",
        user_sid()?
    ))?;
    let mut descriptor = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(error("Build workspace security descriptor"));
    }
    Ok(LocalAllocation(descriptor))
}
fn descriptor_text(descriptor: PSECURITY_DESCRIPTOR) -> Result<String> {
    let mut text = null_mut();
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            1,
            SECURITY_INFO,
            &mut text,
            null_mut(),
        )
    } == 0
    {
        return Err(error("Describe workspace security"));
    }
    let _allocation = LocalAllocation(text.cast());
    Ok(unsafe { read_wide(text) }.to_string_lossy().into_owned())
}
fn workspace_acl(workspace: &Workspace) -> Result<String> {
    let mut descriptor = null_mut();
    let status = unsafe {
        GetSecurityInfo(
            workspace.root.as_raw_handle(),
            SE_FILE_OBJECT,
            SECURITY_INFO,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(WorkerError::new(
            "sandbox_workspace_unprepared",
            format!("Cannot verify workspace ACL: Windows error {status}"),
        ));
    }
    let descriptor = LocalAllocation(descriptor);
    // SetSecurityInfo adds AUTO_INHERITED bookkeeping to a protected DACL and
    // to the label. It changes neither the ACEs nor the inheritance protection.
    // Ignore only those exact header annotations; compare every ACE verbatim.
    let normalize = |text: String| text.replace("D:PAI(", "D:P(").replace("S:AI(", "S:(");
    let actual = normalize(descriptor_text(descriptor.0)?);
    let expected = normalize(descriptor_text(expected_descriptor(workspace)?.0)?);
    if actual != expected {
        return Err(WorkerError::new(
            "sandbox_workspace_unprepared",
            "Workspace ACL differs from the dedicated LPAC policy; use sandbox create-workspace with a new path",
        ));
    }
    Ok(fingerprint(&actual))
}

/// Only a new empty directory is modified. Profiles and ACLs remain associated
/// with this directory until the host explicitly removes the dedicated workspace.
pub(super) async fn create_workspace(path: &Path) -> Result<Workspace> {
    let workspace = super::workspace_windows::create_native_root(path).await?;
    let mut created_profiles = Vec::new();
    let result = (|| {
        for writable in [false, true] {
            let name = wide(profile_name(&workspace, writable))?;
            let mut sid = null_mut();
            let hr = unsafe {
                CreateAppContainerProfile(
                    name.as_ptr(),
                    name.as_ptr(),
                    name.as_ptr(),
                    null(),
                    0,
                    &mut sid,
                )
            };
            if hr < 0 {
                return Err(WorkerError::new(
                    "sandbox_unavailable",
                    format!("Create workspace AppContainer profile failed: {hr:#x}"),
                ));
            }
            drop(Sid(sid));
            created_profiles.push(name);
        }
        let descriptor = expected_descriptor(&workspace)?;
        let mut dacl = null_mut();
        let mut sacl = null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        if unsafe {
            GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
        } == 0
            || present == 0
        {
            return Err(error("Read planned DACL"));
        }
        if unsafe {
            GetSecurityDescriptorSacl(descriptor.0, &mut present, &mut sacl, &mut defaulted)
        } == 0
            || present == 0
        {
            return Err(error("Read planned integrity label"));
        }
        // ReOpenFile does not support all directory handles. The existing root
        // and ancestor handles deny delete sharing, and the reopened object's
        // native ID must match before any ACL change.
        let path = wide(&workspace.path)?;
        let handle = unsafe {
            take_handle(CreateFileW(
                path.as_ptr(),
                READ_CONTROL | WRITE_DAC | WRITE_OWNER,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            ))?
        };
        let mut original: FILE_ID_INFO = unsafe { zeroed() };
        let mut reopened: FILE_ID_INFO = unsafe { zeroed() };
        for (handle, output) in [
            (workspace.root.as_raw_handle(), &mut original),
            (raw(&handle), &mut reopened),
        ] {
            if unsafe {
                GetFileInformationByHandleEx(
                    handle,
                    FileIdInfo,
                    (output as *mut FILE_ID_INFO).cast(),
                    size_of::<FILE_ID_INFO>() as u32,
                )
            } == 0
            {
                return Err(error("Revalidate workspace ACL target"));
            }
        }
        if original.VolumeSerialNumber != reopened.VolumeSerialNumber
            || original.FileId.Identifier != reopened.FileId.Identifier
        {
            return Err(WorkerError::new(
                "workspace_conflict",
                "Workspace ACL target identity changed",
            ));
        }
        let status = unsafe {
            SetSecurityInfo(
                raw(&handle),
                SE_FILE_OBJECT,
                SECURITY_INFO | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                dacl,
                sacl,
            )
        };
        if status != 0 {
            return Err(WorkerError::new(
                "sandbox_unavailable",
                format!("Provision dedicated workspace ACL failed: Windows error {status}"),
            ));
        }
        workspace_acl(&workspace)?;
        Ok(())
    })();
    if let Err(error) = result {
        for name in created_profiles {
            unsafe {
                DeleteAppContainerProfile(name.as_ptr());
            }
        }
        // Keep failures non-destructive. The new empty root can be removed by
        // the host; never remove a name after releasing its identity pin.
        return Err(error);
    }
    Ok(workspace)
}

pub(super) fn remove_workspace_profiles(workspace: &Workspace) -> Result<()> {
    for writable in [false, true] {
        let name = wide(profile_name(workspace, writable))?;
        let hr = unsafe { DeleteAppContainerProfile(name.as_ptr()) };
        if hr < 0 {
            return Err(WorkerError::new(
                "cleanup_failed",
                format!("Delete dedicated AppContainer profile failed: {hr:#x}"),
            ));
        }
    }
    Ok(())
}

pub(super) fn description(
    workspace: &Workspace,
    writable: bool,
    limits: &ProcessLimits,
) -> Result<Value> {
    validate_limits(limits)?;
    let acl = workspace_acl(workspace)?;
    let profile = profile_path(&Sid::derive(&profile_name(workspace, writable))?)?;
    Ok(
        json!({"backend":"windows-lpac-job", "version":POLICY_VERSION,
        "workspace_identity":workspace.identity, "workspace_path":workspace.path,
        "workspace_writable":writable,"workspace_acl":acl,
        "network":"no-capabilities", "ipc":"lpac-object-namespace-and-acl; win32k disabled",
        "runtime":"Resources whose current ACLs permit this LPAC or ALL RESTRICTED APPLICATION PACKAGES; executables from System32 or workspace",
        "profile":profile,"temporary":"Windows-managed profile Temp directory; shared only by this workspace/mode and persistent until profile revocation",
        "inheritance":"child process creation denied; non-breakaway job with one active process", "cleanup":"kernel kill-on-close job; wait for zero active processes",
        "resource_limits":limits, "memory_scope":"aggregate job commit", "cpu_scope":"aggregate job user time",
        "effective_process_limit":1,
        "limitations":["Existing host ACLs can pre-grant non-workspace resources; there is no per-path deny overlay", "child processes and multiprocess toolchains are unsupported", "container profile and temporary state persist per workspace/mode until explicit revocation", "no per-file size or handle-count limit", "no browser support"]}),
    )
}
fn validate_limits(limits: &ProcessLimits) -> Result<()> {
    if limits.timeout_ms == 0
        || limits.memory_bytes == 0
        || limits.cpu_seconds == 0
        || limits.processes == 0
        || limits.output_bytes == 0
        || limits.processes > u32::MAX as u64
        || limits.memory_bytes > usize::MAX as u64
        || limits.cpu_seconds > i64::MAX as u64 / 10_000_000
        || std::time::Instant::now()
            .checked_add(Duration::from_millis(limits.timeout_ms))
            .is_none()
    {
        return Err(WorkerError::new(
            "resource_limit",
            "Windows job requires finite nonzero representable limits",
        ));
    }
    Ok(())
}
fn system_directory() -> Result<PathBuf> {
    let mut path = [0u16; 32768];
    let len = unsafe { GetSystemDirectoryW(path.as_mut_ptr(), path.len() as u32) } as usize;
    if len == 0 || len >= path.len() {
        return Err(error("Resolve Windows system directory"));
    }
    Ok(PathBuf::from(OsString::from_wide(&path[..len])))
}
pub(super) fn probe_argv() -> Result<Vec<String>> {
    Ok(vec![
        system_directory()?
            .join("cmd.exe")
            .to_string_lossy()
            .into_owned(),
        "/d".into(),
        "/c".into(),
        "exit".into(),
        "0".into(),
    ])
}
fn create_job(limits: &ProcessLimits) -> Result<OwnedHandle> {
    let job = unsafe { take_handle(CreateJobObjectW(null(), null()))? };
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_JOB_TIME;
    // The supported zero-capability LPAC mode is deliberately single-process.
    // A requested process upper bound never permits child process creation.
    info.BasicLimitInformation.ActiveProcessLimit = 1;
    info.BasicLimitInformation.PerJobUserTimeLimit = (limits.cpu_seconds * 10_000_000) as i64;
    info.JobMemoryLimit = limits.memory_bytes as usize;
    if unsafe {
        SetInformationJobObject(
            raw(&job),
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(error("Apply mandatory job limits"));
    }
    let mut applied: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    if unsafe {
        QueryInformationJobObject(
            raw(&job),
            JobObjectExtendedLimitInformation,
            (&mut applied as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            null_mut(),
        )
    } == 0
    {
        return Err(error("Verify mandatory job limits"));
    }
    if applied.BasicLimitInformation.LimitFlags & info.BasicLimitInformation.LimitFlags
        != info.BasicLimitInformation.LimitFlags
        || applied.BasicLimitInformation.ActiveProcessLimit != 1
        || applied.JobMemoryLimit != info.JobMemoryLimit
        || applied.BasicLimitInformation.PerJobUserTimeLimit
            != info.BasicLimitInformation.PerJobUserTimeLimit
    {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "Windows did not apply the required job limits",
        ));
    }
    // Job UI limits prohibit nesting in existing CI/host jobs. Win32k lockdown
    // below blocks GUI system calls at creation while preserving job nesting.
    Ok(job)
}
struct Attributes {
    _storage: Vec<usize>,
    pointer: LPPROC_THREAD_ATTRIBUTE_LIST,
}
impl Attributes {
    fn new() -> Result<Self> {
        let mut size = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 5, 0, &mut size);
        }
        let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
        let pointer = storage.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(pointer, 5, 0, &mut size) } == 0 {
            return Err(error("Initialize process attributes"));
        }
        Ok(Self {
            _storage: storage,
            pointer,
        })
    }
    fn set<T>(&mut self, attribute: u32, value: &T) -> Result<()> {
        if unsafe {
            UpdateProcThreadAttribute(
                self.pointer,
                0,
                attribute as usize,
                (value as *const T).cast(),
                size_of::<T>(),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(error("Apply mandatory process attribute"));
        }
        Ok(())
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.pointer);
        }
    }
}
fn pipe(parent_reads: bool) -> Result<(OwnedHandle, OwnedHandle)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let mut read = null_mut();
    let mut write = null_mut();
    if unsafe { CreatePipe(&mut read, &mut write, &sa, 0) } == 0 {
        return Err(error("Create isolated standard I/O"));
    }
    let read = unsafe { take_handle(read)? };
    let write = unsafe { take_handle(write)? };
    let (parent, child) = if parent_reads {
        (read, write)
    } else {
        (write, read)
    };
    if unsafe { SetHandleInformation(raw(&parent), HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(error("Prevent parent pipe inheritance"));
    }
    Ok((parent, child))
}

/// Quote according to the Windows CRT argv convention; never invoke a shell.
fn quote_arg(value: &OsStr) -> Vec<u16> {
    let plain: Vec<u16> = value.encode_wide().collect();
    if !plain.is_empty() && !plain.iter().any(|c| matches!(*c, 9 | 32 | 34)) {
        return plain;
    }
    let mut result = vec![b'"' as u16];
    let mut slashes = 0;
    for ch in value.encode_wide() {
        if ch == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        if ch == b'"' as u16 {
            result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
        } else {
            result.extend(std::iter::repeat_n(b'\\' as u16, slashes));
        }
        slashes = 0;
        result.push(ch);
    }
    result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    result.push(b'"' as u16);
    result
}
struct Child {
    // The job is never inheritable and is in the atomic creation attribute list.
    job: OwnedHandle,
    process: OwnedHandle,
    stdout: Option<OwnedHandle>,
    stderr: Option<OwnedHandle>,
    stdin: Option<OwnedHandle>,
}
impl Child {
    fn terminate(&self) -> Result<()> {
        if unsafe { TerminateJobObject(raw(&self.job), 1) } == 0 {
            return Err(error("Terminate sandbox process tree"));
        }
        Ok(())
    }
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
            return Err(error("Verify process tree cleanup"));
        }
        Ok(info.ActiveProcesses)
    }
    fn exit_code(&self) -> Result<Option<i32>> {
        match unsafe { WaitForSingleObject(raw(&self.process), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(raw(&self.process), &mut code) } == 0 {
                    return Err(error("Read process result"));
                }
                Ok(Some(code as i32))
            }
            _ => Err(error("Observe native process")),
        }
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        // Covers dropped SDK futures and startup failures. KILL_ON_JOB_CLOSE is
        // additionally enforced by the kernel if the host itself exits.
        let _ = self.terminate();
        while self.active().is_ok_and(|n| n != 0) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn spawn(
    sandbox: &Sandbox,
    workspace: &Workspace,
    writable: bool,
    argv: &[String],
    cwd: &str,
) -> Result<Child> {
    description(workspace, writable, &sandbox.limits)?;
    let system = system_directory()?;
    let program = Path::new(&argv[0]);
    let program = if program.components().count() == 1 {
        system.join(program)
    } else if program.is_absolute() {
        program.to_owned()
    } else {
        workspace.path.join(program)
    };
    let program = std::fs::canonicalize(program)?;
    let system_canonical = std::fs::canonicalize(&system)?;
    if !program.starts_with(&workspace.path) && !program.starts_with(system_canonical) {
        return Err(WorkerError::new(
            "path_denied",
            "Windows sandbox executables must be in System32 or the dedicated workspace",
        ));
    }
    if !program
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    {
        return Err(WorkerError::new(
            "invalid_argv",
            "Specify an executable .exe; batch scripts require an explicitly selected shell",
        ));
    }
    let cwd = workspace.path.join(cwd);
    let sid = Sid::derive(&profile_name(workspace, writable))?;
    let profile = profile_path(&sid)?;
    let local_app_data = local_app_data()?;
    let temporary = profile.join("Temp");
    let job = create_job(&sandbox.limits)?;
    let (stdout, child_stdout) = pipe(true)?;
    let (stderr, child_stderr) = pipe(true)?;
    let (stdin, child_stdin) = pipe(false)?;
    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: sid.0,
        Capabilities: null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let jobs = [raw(&job)];
    let handles = [raw(&child_stdout), raw(&child_stderr), raw(&child_stdin)];
    let mut attributes = Attributes::new()?;
    attributes.set(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &capabilities)?;
    attributes.set(
        PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
        &OPT_OUT_ALL_APPLICATION_PACKAGES,
    )?;
    attributes.set(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
    attributes.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)?;
    attributes.set(
        PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
        &WIN32K_SYSTEM_CALL_DISABLE,
    )?;
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = raw(&child_stdin);
    startup.StartupInfo.hStdOutput = raw(&child_stdout);
    startup.StartupInfo.hStdError = raw(&child_stderr);
    startup.lpAttributeList = attributes.pointer;
    let mut command = quote_arg(program.as_os_str());
    for argument in &argv[1..] {
        command.push(b' ' as u16);
        command.extend(quote_arg(OsStr::new(argument)));
    }
    command.push(0);
    if command.len() > 32767 {
        return Err(WorkerError::new(
            "invalid_argv",
            "Windows command line exceeds 32767 UTF-16 units",
        ));
    }
    let windows = system
        .parent()
        .ok_or_else(|| WorkerError::new("sandbox_unavailable", "Windows directory unavailable"))?;
    let mut env = Vec::new();
    for (key, value) in [
        // Windows rewrites LOCALAPPDATA/TEMP/TMP for an AppContainer during
        // initialization. Its input base must come from the trusted Known
        // Folder API, not the host's mutable environment or a made-up path.
        ("LOCALAPPDATA", local_app_data.as_path()),
        ("PATH", system.as_path()),
        ("SystemRoot", windows),
        ("TEMP", temporary.as_path()),
        ("TMP", temporary.as_path()),
        ("USERPROFILE", profile.as_path()),
        ("WINDIR", windows),
    ] {
        env.extend(OsStr::new(key).encode_wide());
        env.push(b'=' as u16);
        env.extend(value.as_os_str().encode_wide());
        env.push(0);
    }
    env.push(0);
    let program_wide = wide(&program)?;
    let cwd_wide = wide(&cwd)?;
    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    if unsafe {
        CreateProcessW(
            program_wide.as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_NO_WINDOW
                | CREATE_SUSPENDED,
            env.as_ptr().cast(),
            cwd_wide.as_ptr(),
            &startup.StartupInfo,
            &mut info,
        )
    } == 0
    {
        return Err(error(
            "Create LPAC process with atomic job assignment (no fallback)",
        ));
    }
    let process = unsafe { take_handle(info.hProcess)? };
    let thread = unsafe { take_handle(info.hThread)? };
    let child = Child {
        job,
        process,
        stdout: Some(stdout),
        stderr: Some(stderr),
        stdin: Some(stdin),
    };
    let mut in_job = 0;
    if unsafe { IsProcessInJob(raw(&child.process), raw(&child.job), &mut in_job) } == 0
        || in_job == 0
    {
        return Err(WorkerError::new(
            "sandbox_unavailable",
            "Suspended process is not assigned to the required job",
        ));
    }
    verify_token(raw(&child.process), &sid)?;
    if unsafe { ResumeThread(raw(&thread)) } == u32::MAX {
        return Err(error("Resume isolated child"));
    }
    Ok(child)
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
    relative_path(cwd)?;
    if argv.is_empty()
        || argv.len() > 128
        || argv.iter().any(|a| a.len() > 8192 || a.contains('\0'))
    {
        return Err(WorkerError::new("invalid_argv", "Invalid program/argv"));
    }
    if cancel.is_cancelled() {
        return Err(WorkerError::new(
            "cancelled",
            "Process cancelled before start",
        ));
    }
    workspace.revalidate().await?;
    // The root is already pinned and revalidated. For a nested cwd, validate
    // its components through the secure file backend before CreateProcess.
    if cwd != "." {
        workspace.list(cwd, 20_000).await?;
    }
    if cancel.is_cancelled() {
        return Err(WorkerError::new(
            "cancelled",
            "Process cancelled before start",
        ));
    }
    let mut child = spawn(sandbox, workspace, writable, argv, cwd)?;
    let out = tokio::fs::File::from_std(File::from(child.stdout.take().unwrap()));
    let err = tokio::fs::File::from_std(File::from(child.stderr.take().unwrap()));
    let mut input = tokio::fs::File::from_std(File::from(child.stdin.take().unwrap()));
    let content = stdin.unwrap_or_default().as_bytes().to_vec();
    let input_task = tokio::spawn(async move {
        let result = input.write_all(&content).await;
        drop(input);
        result
    });
    let out_task = tokio::spawn(drain_live(
        out,
        sandbox.limits.output_bytes,
        live.clone(),
        false,
    ));
    let err_task = tokio::spawn(drain_live(err, sandbox.limits.output_bytes, live, true));
    let mut cancelled = false;
    let mut timed_out = false;
    let deadline = tokio::time::sleep(Duration::from_millis(sandbox.limits.timeout_ms));
    tokio::pin!(deadline);
    let exit_code = loop {
        if let Some(code) = child.exit_code()? {
            break code;
        }
        tokio::select! {
            _ = cancel.cancelled() => { cancelled = true; child.terminate()?; break 1; },
            _ = &mut deadline => { timed_out = true; child.terminate()?; break 1; },
            _ = tokio::time::sleep(Duration::from_millis(5)) => {},
        }
    };
    // Normal leader exit is not tree completion. Kill descendants, including
    // detached processes, then keep the caller's lease until the job is empty.
    child.terminate()?;
    while child.active()? != 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let input_result = input_task
        .await
        .map_err(|_| WorkerError::new("process_io", "Input worker failed"))?;
    if let Err(error) = input_result
        && error.kind() != std::io::ErrorKind::BrokenPipe
        && !cancelled
        && !timed_out
    {
        return Err(WorkerError::new(
            "process_io",
            "Could not deliver process input",
        ));
    }
    let (stdout, ot) = out_task
        .await
        .map_err(|_| WorkerError::new("process_io", "Output worker failed"))??;
    let (stderr, et) = err_task
        .await
        .map_err(|_| WorkerError::new("process_io", "Output worker failed"))??;
    Ok(ProcessOutput {
        exit_code: Some(exit_code),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated: ot || et,
        cancelled,
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn argv_preserves_empty_quotes_backslashes_and_unicode() {
        for (value, expected) in [
            ("", "\"\""),
            ("a b", "\"a b\""),
            ("x \\", "\"x \\\\\""),
            ("a\"b", "\"a\\\"b\""),
            ("Grüße 世界", "\"Grüße 世界\""),
        ] {
            assert_eq!(
                String::from_utf16(&quote_arg(OsStr::new(value))).unwrap(),
                expected
            );
        }
    }
}
