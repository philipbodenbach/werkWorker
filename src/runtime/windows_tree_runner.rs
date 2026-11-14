//! Trusted entry point for a provisioned dedicated-account process tree.
//! The host establishes the account and Job before this executable runs. This
//! runner authenticates its pipes and starts only the host's bounded request.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, c_void},
    fs::File,
    io::{self, Read, Write},
    mem::{size_of, zeroed},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::*,
    System::{
        Diagnostics::ToolHelp::*, JobObjects::*, Pipes::GetNamedPipeServerProcessId,
        SystemServices::PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY, Threading::*,
    },
};

const CONTROL_LIMIT: usize = 64 * 1024;
const WIN32K_SYSTEM_CALL_DISABLE: u64 = 1 << 28;
const RUNNER_ERROR: i32 = 125;
const LOGON_ID: u32 = 0xc000_0000;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    pub version: u32,
    pub host_sid: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub environment: BTreeMap<String, String>,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn native_error(operation: &'static str) -> io::Error {
    io::Error::other(format!("{operation}: {}", io::Error::last_os_error()))
}
fn wide(value: impl AsRef<OsStr>) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = value.as_ref().encode_wide().collect();
    if value.contains(&0) {
        return Err(invalid("NUL in native runner argument"));
    }
    value.push(0);
    Ok(value)
}
fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
}
unsafe fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(native_error("Create native runner handle"));
    }
    // SAFETY: callers transfer a newly owned, non-pseudo handle exactly once.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn arguments() -> io::Result<(String, u32)> {
    let values: Vec<_> = std::env::args_os().skip(1).collect();
    if values.len() != 4 || values[0] != "--pipe-prefix" || values[2] != "--host-pid" {
        return Err(invalid("Expected --pipe-prefix PIPE --host-pid PID"));
    }
    let prefix = values[1]
        .to_str()
        .ok_or_else(|| invalid("Pipe prefix must be Unicode"))?;
    let suffix = prefix
        .strip_prefix(r"\\.\pipe\werk-tree-")
        .ok_or_else(|| invalid("Invalid runner pipe namespace"))?;
    // Keep this local-only name distinct from paths, UNC servers and aliases.
    if suffix.len() != 36 || uuid::Uuid::parse_str(suffix).is_err() {
        return Err(invalid("Runner pipe name must end in a canonical UUID"));
    }
    let host = values[3]
        .to_str()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid != 0)
        .ok_or_else(|| invalid("Invalid trusted host process ID"))?;
    Ok((prefix.to_owned(), host))
}

fn open_pipe(
    prefix: &str,
    suffix: &str,
    access: u32,
    inherit: bool,
    host: u32,
) -> io::Result<OwnedHandle> {
    let path = wide(format!("{prefix}.{suffix}"))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: i32::from(inherit),
    };
    // Identification prevents a pipe server from impersonating this account.
    let handle = unsafe {
        owned(CreateFileW(
            path.as_ptr(),
            access,
            0,
            &attributes,
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            null_mut(),
        ))?
    };
    let mut server = 0;
    if unsafe { GetNamedPipeServerProcessId(raw(&handle), &mut server) } == 0 {
        return Err(native_error("Authenticate native runner pipe"));
    }
    if server != host {
        return Err(invalid("Runner pipe belongs to a different host process"));
    }
    let mut flags = 0;
    if unsafe { GetHandleInformation(raw(&handle), &mut flags) } == 0 {
        return Err(native_error("Inspect native pipe inheritance"));
    }
    if (flags & HANDLE_FLAG_INHERIT != 0) != inherit {
        return Err(invalid("Unexpected native pipe inheritance"));
    }
    Ok(handle)
}

fn verify_host_containment() -> io::Result<()> {
    let process = unsafe { GetCurrentProcess() };
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(native_error("Inspect runner account"));
    }
    let token = unsafe { owned(token)? };
    let mut elevation: TOKEN_ELEVATION = unsafe { zeroed() };
    let mut size = 0;
    if unsafe {
        GetTokenInformation(
            raw(&token),
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    } == 0
    {
        return Err(native_error("Query runner elevation"));
    }
    if elevation.TokenIsElevated != 0 {
        return Err(invalid("An elevated account cannot execute the runner"));
    }
    let mut member = 0;
    if unsafe { IsProcessInJob(process, null_mut(), &mut member) } == 0 {
        return Err(native_error("Inspect runner Job membership"));
    }
    if member == 0 {
        return Err(invalid("Runner was not assigned to a Job before execution"));
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    if unsafe {
        QueryInformationJobObject(
            null_mut(),
            JobObjectExtendedLimitInformation,
            (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            null_mut(),
        )
    } == 0
    {
        return Err(native_error("Inspect runner Job limits"));
    }
    let required = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_JOB_TIME;
    let basic = &limits.BasicLimitInformation;
    if basic.LimitFlags & required != required
        || basic.LimitFlags & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK)
            != 0
        || basic.ActiveProcessLimit < 2
        || limits.JobMemoryLimit == 0
        || basic.PerJobUserTimeLimit <= 0
    {
        return Err(invalid(
            "Runner Job lacks mandatory finite non-breakaway limits",
        ));
    }
    Ok(())
}

fn read_request(control: OwnedHandle) -> io::Result<Request> {
    let mut bytes = Vec::new();
    let mut control = File::from(control);
    let mut chunk = [0u8; 4096];
    loop {
        match control.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                if bytes.len() + count > CONTROL_LIMIT {
                    return Err(invalid("Runner control request exceeds 64 KiB"));
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    drop(control);
    let request: Request = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("Malformed native runner control request"))?;
    if request.version != 1
        || request.host_sid.len() > 184
        || request.host_sid.is_empty()
        || request.argv.is_empty()
        || request.argv.len() > 128
        || request
            .argv
            .iter()
            .any(|value| value.len() > 8192 || value.contains('\0'))
        || !Path::new(&request.argv[0]).is_absolute()
        || !Path::new(&request.argv[0])
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        || !request.cwd.is_absolute()
    {
        return Err(invalid(
            "Invalid native runner request version, argv or working directory",
        ));
    }
    Ok(request)
}

/// Windows CRT argument quoting; lpApplicationName is always supplied explicitly.
fn quote_arg(value: &OsStr) -> Vec<u16> {
    let plain: Vec<u16> = value.encode_wide().collect();
    if !plain.is_empty() && !plain.iter().any(|c| matches!(*c, 9 | 32 | 34)) {
        return plain;
    }
    let mut quoted = vec![b'"' as u16];
    let mut slashes = 0;
    for ch in value.encode_wide() {
        if ch == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        quoted.extend(std::iter::repeat_n(
            b'\\' as u16,
            if ch == b'"' as u16 {
                slashes * 2 + 1
            } else {
                slashes
            },
        ));
        slashes = 0;
        quoted.push(ch);
    }
    quoted.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    quoted.push(b'"' as u16);
    quoted
}
fn environment(values: &BTreeMap<String, String>) -> io::Result<Vec<u16>> {
    let mut entries: Vec<_> = values.iter().collect();
    entries.sort_by_key(|(name, _)| name.to_ascii_uppercase());
    let mut previous = None;
    let mut block = Vec::new();
    for (name, value) in entries {
        let folded = name.to_ascii_uppercase();
        if name.is_empty()
            || !name.is_ascii()
            || name.contains(['=', '\0'])
            || value.contains('\0')
            || previous.as_ref() == Some(&folded)
        {
            return Err(invalid("Invalid or duplicate native environment variable"));
        }
        previous = Some(folded);
        block.extend(wide(format!("{name}={value}"))?);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    if block.len() > 32767 {
        return Err(invalid("Native environment exceeds 32767 UTF-16 units"));
    }
    Ok(block)
}

struct Attributes {
    _storage: Vec<usize>,
    pointer: LPPROC_THREAD_ATTRIBUTE_LIST,
}
impl Attributes {
    fn new() -> io::Result<Self> {
        Self::with_capacity(2)
    }
    fn with_capacity(count: u32) -> io::Result<Self> {
        let mut size = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), count, 0, &mut size);
        }
        if size == 0 {
            return Err(native_error("Size native process attributes"));
        }
        let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
        let pointer = storage.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(pointer, count, 0, &mut size) } == 0 {
            return Err(native_error("Initialize native process attributes"));
        }
        Ok(Self {
            _storage: storage,
            pointer,
        })
    }
    fn set<T>(&mut self, attribute: u32, value: &T) -> io::Result<()> {
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
            return Err(native_error("Apply native process attribute"));
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
struct Payload {
    process: OwnedHandle,
    thread: OwnedHandle,
    finished: bool,
}
impl Drop for Payload {
    fn drop(&mut self) {
        if !self.finished {
            // Only the retained CreateProcess handle is used, never a reopened
            // PID. Host timeout/termination still owns the containing Job.
            unsafe {
                TerminateProcess(raw(&self.process), RUNNER_ERROR as u32);
                WaitForSingleObject(raw(&self.process), INFINITE);
            }
        }
    }
}

struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn token_information(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<usize>> {
    let mut bytes = 0;
    unsafe {
        GetTokenInformation(token, class, null_mut(), 0, &mut bytes);
    }
    if bytes == 0 || bytes > 1024 * 1024 {
        return Err(native_error("Size payload token information"));
    }
    let mut storage = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    if unsafe { GetTokenInformation(token, class, storage.as_mut_ptr().cast(), bytes, &mut bytes) }
        == 0
    {
        return Err(native_error("Read payload token information"));
    }
    Ok(storage)
}
fn token_groups(storage: &[usize]) -> io::Result<&[SID_AND_ATTRIBUTES]> {
    let count = unsafe { *storage.as_ptr().cast::<u32>() } as usize;
    let offset = std::mem::offset_of!(TOKEN_GROUPS, Groups);
    if count > 4096
        || offset + count * size_of::<SID_AND_ATTRIBUTES>() > std::mem::size_of_val(storage)
    {
        return Err(invalid("Malformed native token SID list"));
    }
    Ok(unsafe {
        std::slice::from_raw_parts(storage.as_ptr().cast::<u8>().add(offset).cast(), count)
    })
}
fn sid_text(sid: PSID) -> io::Result<String> {
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(invalid("Invalid token SID"));
    }
    let mut pointer = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut pointer) } == 0 {
        return Err(native_error("Format token SID"));
    }
    let _memory = LocalAllocation(pointer.cast());
    let mut length = 0;
    unsafe {
        while *pointer.add(length) != 0 {
            length += 1;
        }
    }
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(pointer, length)
    }))
}
fn parse_sid(text: &str) -> io::Result<LocalAllocation> {
    let mut sid = null_mut();
    if unsafe { ConvertStringSidToSidW(wide(text)?.as_ptr(), &mut sid) } == 0 {
        return Err(native_error("Parse trusted host SID"));
    }
    Ok(LocalAllocation(sid))
}
fn change_notify_privilege() -> io::Result<LUID> {
    let mut luid = LUID::default();
    if unsafe {
        LookupPrivilegeValueW(null(), wide("SeChangeNotifyPrivilege")?.as_ptr(), &mut luid)
    } == 0
    {
        return Err(native_error("Resolve traversal privilege"));
    }
    Ok(luid)
}
fn verify_restricted_token(token: HANDLE) -> io::Result<()> {
    if unsafe { IsTokenRestricted(token) } == 0 {
        return Err(invalid("Payload token lacks restricting SIDs"));
    }
    let groups = token_information(token, TokenRestrictedSids)?;
    if token_groups(&groups)?.len() != 3 {
        return Err(invalid("Payload token has unexpected restricting SIDs"));
    }
    let elevation = token_information(token, TokenElevation)?;
    if unsafe { *elevation.as_ptr().cast::<u32>() } != 0 {
        return Err(invalid("Payload token is elevated"));
    }
    let privileges = token_information(token, TokenPrivileges)?;
    let count = unsafe { *privileges.as_ptr().cast::<u32>() } as usize;
    let offset = std::mem::offset_of!(TOKEN_PRIVILEGES, Privileges);
    if count > 512
        || offset + count * size_of::<LUID_AND_ATTRIBUTES>()
            > std::mem::size_of_val(privileges.as_slice())
    {
        return Err(invalid("Malformed payload privilege list"));
    }
    let privileges = unsafe {
        std::slice::from_raw_parts(
            privileges
                .as_ptr()
                .cast::<u8>()
                .add(offset)
                .cast::<LUID_AND_ATTRIBUTES>(),
            count,
        )
    };
    let change_notify = change_notify_privilege()?;
    let mut enabled = false;
    for privilege in privileges {
        if privilege.Luid.LowPart != change_notify.LowPart
            || privilege.Luid.HighPart != change_notify.HighPart
        {
            return Err(invalid("Unexpected privilege survived payload restriction"));
        }
        enabled |= privilege.Attributes & SE_PRIVILEGE_ENABLED != 0;
    }
    if !enabled {
        return Err(invalid("Payload lacks enabled traversal privilege"));
    }
    Ok(())
}
/// Codex-style write restriction for a dedicated account. Ordinary ACL read
/// grants and Everyone write grants remain effective; this is not a read allowlist.
fn restricted_payload_token(host_sid: &str) -> io::Result<OwnedHandle> {
    let host = parse_sid(host_sid)?;
    let host = sid_text(host.0)?;
    let mut base = null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY
                | TOKEN_DUPLICATE
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_PRIVILEGES,
            &mut base,
        )
    } == 0
    {
        return Err(native_error("Open runner token for restriction"));
    }
    let base = unsafe { owned(base)? };
    let user = token_information(raw(&base), TokenUser)?;
    let user_sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let groups = token_information(raw(&base), TokenGroups)?;
    let logon_sid = token_groups(&groups)?
        .iter()
        .find(|entry| entry.Attributes & LOGON_ID == LOGON_ID)
        .map(|entry| entry.Sid)
        .ok_or_else(|| invalid("Runner token has no logon SID"))?;
    let logon_text = sid_text(logon_sid)?;
    let everyone = parse_sid("S-1-1-0")?;
    let restrictions = [user_sid, logon_sid, everyone.0].map(|sid| SID_AND_ATTRIBUTES {
        Sid: sid,
        Attributes: 0,
    });
    let mut token = null_mut();
    // Values are defined by CreateRestrictedToken / winnt.h. WRITE_RESTRICTED
    // restricts write checks only; native tests must not mistake it for LPAC.
    const FLAGS: u32 = 0x1 | 0x4 | 0x8; // DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED
    if unsafe {
        CreateRestrictedToken(
            raw(&base),
            FLAGS,
            0,
            null(),
            0,
            null(),
            restrictions.len() as u32,
            restrictions.as_ptr(),
            &mut token,
        )
    } == 0
    {
        return Err(native_error("Create restricted dedicated-account token"));
    }
    let token = unsafe { owned(token)? };
    // OWNER RIGHTS removes the shared account owner's implicit WRITE_DAC.
    // The creating logon retains full access. The trusted host can inspect,
    // synchronize and terminate descendants without receiving sandbox handles.
    let descriptor = wide(format!(
        "D:(A;;GA;;;{logon_text})(A;;0x101001;;;{host})(A;;RC;;;OW)"
    ))?;
    let mut sd = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            1,
            &mut sd,
            null_mut(),
        )
    } == 0
    {
        return Err(native_error("Create restricted default object DACL"));
    }
    let _sd = LocalAllocation(sd);
    let mut dacl = null_mut();
    let mut present = 0;
    let mut defaulted = 0;
    if unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) } == 0
        || present == 0
        || dacl.is_null()
    {
        return Err(invalid("Restricted default DACL is absent"));
    }
    let default_dacl = TOKEN_DEFAULT_DACL { DefaultDacl: dacl };
    if unsafe {
        SetTokenInformation(
            raw(&token),
            TokenDefaultDacl,
            (&default_dacl as *const TOKEN_DEFAULT_DACL).cast(),
            size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    } == 0
    {
        return Err(native_error("Set restricted default object DACL"));
    }
    let adjustment = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: change_notify_privilege()?,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    unsafe {
        SetLastError(ERROR_SUCCESS);
    }
    if unsafe { AdjustTokenPrivileges(raw(&token), 0, &adjustment, 0, null_mut(), null_mut()) } == 0
        || unsafe { GetLastError() } != ERROR_SUCCESS
    {
        return Err(native_error("Enable payload traversal privilege"));
    }
    verify_restricted_token(raw(&token))?;
    Ok(token)
}
fn verify_payload_token(process: HANDLE, expected: HANDLE) -> io::Result<()> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(native_error("Inspect suspended payload token"));
    }
    let token = unsafe { owned(token)? };
    verify_restricted_token(raw(&token))?;
    let expected_user = token_information(expected, TokenUser)?;
    let actual_user = token_information(raw(&token), TokenUser)?;
    if unsafe {
        EqualSid(
            (*expected_user.as_ptr().cast::<TOKEN_USER>()).User.Sid,
            (*actual_user.as_ptr().cast::<TOKEN_USER>()).User.Sid,
        )
    } == 0
    {
        return Err(invalid(
            "Suspended payload user differs from the restricted token",
        ));
    }
    let expected_groups = token_information(expected, TokenRestrictedSids)?;
    let actual_groups = token_information(raw(&token), TokenRestrictedSids)?;
    for expected in token_groups(&expected_groups)? {
        if !token_groups(&actual_groups)?
            .iter()
            .any(|actual| unsafe { EqualSid(actual.Sid, expected.Sid) } != 0)
        {
            return Err(invalid("Suspended payload restricting SID differs"));
        }
    }
    Ok(())
}

fn object_dacl(descriptor: &str) -> io::Result<(LocalAllocation, *mut ACL)> {
    let mut sd = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(descriptor)?.as_ptr(),
            1,
            &mut sd,
            null_mut(),
        )
    } == 0
    {
        return Err(native_error("Create trusted runner object DACL"));
    }
    let memory = LocalAllocation(sd);
    let mut dacl = null_mut();
    let mut present = 0;
    let mut defaulted = 0;
    if unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) } == 0
        || present == 0
        || dacl.is_null()
    {
        return Err(invalid("Trusted runner object DACL is absent"));
    }
    Ok((memory, dacl))
}
fn kernel_dacl_matches(handle: HANDLE, dacl: *const ACL) -> io::Result<bool> {
    let mut actual_dacl = null_mut();
    let mut actual_sd = null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut actual_dacl,
            null_mut(),
            &mut actual_sd,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let _actual_sd = LocalAllocation(actual_sd);
    Ok(!actual_dacl.is_null()
        && unsafe { (*actual_dacl).AclSize == (*dacl).AclSize }
        && unsafe {
            std::slice::from_raw_parts(actual_dacl.cast::<u8>(), (*actual_dacl).AclSize as usize)
                == std::slice::from_raw_parts(dacl.cast::<u8>(), (*dacl).AclSize as usize)
        })
}
fn seal_kernel_object(handle: HANDLE, descriptor: &str) -> io::Result<()> {
    let (_memory, dacl) = object_dacl(descriptor)?;
    let status = unsafe {
        SetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    if !kernel_dacl_matches(handle, dacl)? {
        return Err(invalid("Trusted runner object DACL did not persist"));
    }
    Ok(())
}

fn seal_runner_threads(host: &str) -> io::Result<()> {
    let descriptor = format!(
        "D:P(A;;{THREAD_ALL_ACCESS:#x};;;SY)(A;;{THREAD_ALL_ACCESS:#x};;;{host})(A;;RC;;;OW)"
    );
    let (_memory, dacl) = object_dacl(&descriptor)?;
    let snapshot = unsafe { owned(CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0))? };
    let mut entry: THREADENTRY32 = unsafe { zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    if unsafe { Thread32First(raw(&snapshot), &mut entry) } == 0 {
        return Err(native_error("Enumerate trusted runner threads"));
    }
    let pid = unsafe { GetCurrentProcessId() };
    let mut count = 0;
    loop {
        if entry.th32OwnerProcessID == pid {
            count += 1;
            let observer = unsafe { owned(OpenThread(READ_CONTROL, 0, entry.th32ThreadID))? };
            // New threads already have the hardened primary-token default.
            // Do not ask them for WRITE_DAC, which that default intentionally denies.
            if !kernel_dacl_matches(raw(&observer), dacl)? {
                let thread = unsafe {
                    owned(OpenThread(
                        READ_CONTROL | WRITE_DAC | THREAD_QUERY_LIMITED_INFORMATION,
                        0,
                        entry.th32ThreadID,
                    ))?
                };
                if unsafe { GetProcessIdOfThread(raw(&thread)) } != pid {
                    return Err(invalid("Runner thread identity changed during protection"));
                }
                seal_kernel_object(raw(&thread), &descriptor)?;
            }
        }
        if unsafe { Thread32Next(raw(&snapshot), &mut entry) } == 0 {
            if unsafe { GetLastError() } != ERROR_NO_MORE_FILES {
                return Err(native_error("Finish trusted runner thread enumeration"));
            }
            break;
        }
    }
    if count == 0 {
        return Err(invalid("Runner thread enumeration was empty"));
    }
    Ok(())
}

/// A payload shares the dedicated account with the trusted runner. Remove
/// account/logon grants and implicit owner WRITE_DAC from the runner's live
/// objects before a payload can execute. Retained handles remain usable.
fn seal_current_runner(host_sid: &str, payload_token: HANDLE) -> io::Result<()> {
    let host = parse_sid(host_sid)?;
    let host = sid_text(host.0)?;
    let user = token_information(payload_token, TokenUser)?;
    let user = sid_text(unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid })?;
    let mut token = null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            READ_CONTROL | WRITE_DAC | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT,
            &mut token,
        )
    } == 0
    {
        return Err(native_error("Open runner token for protection"));
    }
    let token = unsafe { owned(token)? };
    // Set this before the thread snapshot: any thread created concurrently or
    // later starts protected. The separately derived payload token retains its
    // own dedicated-account/logon default DACL.
    let (_default_sd, default_acl) =
        object_dacl(&format!("D:(A;;GA;;;SY)(A;;GA;;;{host})(A;;RC;;;OW)"))?;
    let default_dacl = TOKEN_DEFAULT_DACL {
        DefaultDacl: default_acl,
    };
    if unsafe {
        SetTokenInformation(
            raw(&token),
            TokenDefaultDacl,
            (&default_dacl as *const TOKEN_DEFAULT_DACL).cast(),
            size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    } == 0
    {
        return Err(native_error("Protect future runner object defaults"));
    }
    let actual_default = token_information(raw(&token), TokenDefaultDacl)?;
    let actual_default =
        unsafe { (*actual_default.as_ptr().cast::<TOKEN_DEFAULT_DACL>()).DefaultDacl };
    if actual_default.is_null()
        || unsafe { (*actual_default).AclSize != (*default_acl).AclSize }
        || unsafe {
            std::slice::from_raw_parts(
                actual_default.cast::<u8>(),
                (*actual_default).AclSize as usize,
            ) != std::slice::from_raw_parts(
                default_acl.cast::<u8>(),
                (*default_acl).AclSize as usize,
            )
        }
    {
        return Err(invalid("Trusted runner default DACL did not persist"));
    }
    seal_kernel_object(
        unsafe { GetCurrentProcess() },
        &format!(
            "D:P(A;;{PROCESS_ALL_ACCESS:#x};;;SY)(A;;0x101001;;;{host})(A;;0x101000;;;{user})(A;;RC;;;OW)"
        ),
    )?;
    seal_runner_threads(&host)?;
    seal_kernel_object(
        raw(&token),
        &format!("D:P(A;;{TOKEN_ALL_ACCESS:#x};;;SY)(A;;0x8;;;{host})(A;;RC;;;OW)"),
    )?;
    Ok(())
}
fn execute(
    request: Request,
    input: OwnedHandle,
    output: OwnedHandle,
    errors: &File,
) -> io::Result<i32> {
    let token = restricted_payload_token(&request.host_sid)?;
    seal_current_runner(&request.host_sid, raw(&token))?;
    let executable = wide(&request.argv[0])?;
    let cwd = wide(&request.cwd)?;
    let environment = environment(&request.environment)?;
    let mut command = Vec::new();
    for (index, argument) in request.argv.iter().enumerate() {
        if index != 0 {
            command.push(b' ' as u16);
        }
        command.extend(quote_arg(OsStr::new(argument)));
    }
    command.push(0);
    if command.len() > 32767 {
        return Err(invalid("Native command line exceeds 32767 UTF-16 units"));
    }
    let handles = [raw(&input), raw(&output), errors.as_raw_handle()];
    let mut attributes = Attributes::new()?;
    attributes.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)?;
    attributes.set(
        PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
        &WIN32K_SYSTEM_CALL_DISABLE,
    )?;
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = handles[0];
    startup.StartupInfo.hStdOutput = handles[1];
    startup.StartupInfo.hStdError = handles[2];
    startup.lpAttributeList = attributes.pointer;
    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    if unsafe {
        CreateProcessAsUserW(
            raw(&token),
            executable.as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_SUSPENDED
                | DETACHED_PROCESS
                | EXTENDED_STARTUPINFO_PRESENT
                | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut info,
        )
    } == 0
    {
        return Err(native_error("Create suspended native payload"));
    }
    let mut payload = Payload {
        process: unsafe { owned(info.hProcess)? },
        thread: unsafe { owned(info.hThread)? },
        finished: false,
    };
    verify_payload_token(raw(&payload.process), raw(&token))?;
    let mut member = 0;
    if unsafe { IsProcessInJob(raw(&payload.process), null_mut(), &mut member) } == 0 {
        return Err(native_error("Verify suspended payload Job membership"));
    }
    if member == 0 {
        return Err(invalid("Suspended payload did not inherit a Job"));
    }
    let mut mitigation: PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY = unsafe { zeroed() };
    if unsafe {
        GetProcessMitigationPolicy(
            raw(&payload.process),
            ProcessSystemCallDisablePolicy,
            (&mut mitigation as *mut PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY).cast(),
            size_of::<PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY>(),
        )
    } == 0
    {
        return Err(native_error("Verify suspended payload mitigation"));
    }
    if unsafe { mitigation.Anonymous.Flags } & 1 == 0 {
        return Err(invalid("Suspended payload lacks Win32k lockdown"));
    }
    if unsafe { ResumeThread(raw(&payload.thread)) } != 1 {
        return Err(native_error("Resume verified native payload"));
    }
    drop(input);
    drop(output);
    if unsafe { WaitForSingleObject(raw(&payload.process), INFINITE) } != WAIT_OBJECT_0 {
        return Err(native_error("Wait for native payload"));
    }
    payload.finished = true;
    let mut exit = 0;
    if unsafe { GetExitCodeProcess(raw(&payload.process), &mut exit) } == 0 {
        return Err(native_error("Read native payload exit code"));
    }
    Ok(exit as i32)
}

fn run(stderr: &mut Option<File>) -> io::Result<i32> {
    let (prefix, host) = arguments()?;
    *stderr = Some(File::from(open_pipe(
        &prefix,
        "stderr",
        GENERIC_WRITE,
        true,
        host,
    )?));
    verify_host_containment()?;
    let input = open_pipe(&prefix, "stdin", GENERIC_READ, true, host)?;
    let output = open_pipe(&prefix, "stdout", GENERIC_WRITE, true, host)?;
    let control = open_pipe(&prefix, "control", GENERIC_READ, false, host)?;
    let request = read_request(control)?;
    execute(request, input, output, stderr.as_ref().unwrap())
}

pub(super) fn main() -> i32 {
    let mut stderr = None;
    match run(&mut stderr) {
        Ok(code) => code,
        Err(error) => {
            if let Some(mut output) = stderr {
                let _ = writeln!(output, "Native process-tree runner: {error}");
            } else {
                eprintln!("Native process-tree runner: {error}");
            }
            RUNNER_ERROR
        }
    }
}

#[cfg(test)]
mod restricted_token_tests {
    use super::*;
    #[test]
    fn native_token_restricts_privileges_and_default_objects() {
        let mut base = null_mut();
        assert_ne!(
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut base) },
            0
        );
        let base = unsafe { owned(base).unwrap() };
        let user = token_information(raw(&base), TokenUser).unwrap();
        let host_sid = sid_text(unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid }).unwrap();
        let token = restricted_payload_token(&host_sid).unwrap();
        verify_restricted_token(raw(&token)).unwrap();
        let restricted = token_information(raw(&token), TokenRestrictedSids).unwrap();
        let sids: Vec<_> = token_groups(&restricted)
            .unwrap()
            .iter()
            .map(|entry| sid_text(entry.Sid).unwrap())
            .collect();
        assert!(sids.contains(&host_sid));
        assert!(sids.contains(&"S-1-1-0".to_owned()));
        assert!(sids.iter().any(|sid| sid.starts_with("S-1-5-5-")));
        let dacl = token_information(raw(&token), TokenDefaultDacl).unwrap();
        let dacl = unsafe { (*dacl.as_ptr().cast::<TOKEN_DEFAULT_DACL>()).DefaultDacl };
        assert!(!dacl.is_null());
        assert_eq!(unsafe { (*dacl).AceCount }, 3);
        let mut owner_rights = false;
        let mut host_observer = false;
        for index in 0..3 {
            let mut ace = null_mut();
            assert_ne!(unsafe { GetAce(dacl, index, &mut ace) }, 0);
            let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            let sid = sid_text(std::ptr::addr_of!(ace.SidStart).cast_mut().cast()).unwrap();
            if sid == "S-1-3-4" {
                assert_eq!(ace.Mask, READ_CONTROL);
                owner_rights = true;
            }
            if sid == host_sid {
                assert_eq!(ace.Mask, 0x101001);
                host_observer = true;
            }
        }
        assert!(owner_rights && host_observer);
    }
    #[test]
    fn native_same_account_restricted_process_creation() {
        restricted_process_creation(false);
    }

    #[test]
    fn native_sealed_runner_denies_payload_injection() {
        // Seal only a disposable test subprocess: never alter the test suite's
        // process, thread, or primary-token DACLs.
        let fixture = format!(
            "{}::sealed_runner_fixture",
            module_path!().split_once("::").unwrap().1
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &fixture, "--ignored", "--nocapture"])
            .env("WW_SEALED_RUNNER_FIXTURE", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("SEALED_RUNNER_FIXTURE_PASSED"), "{stdout}");
    }

    #[test]
    #[ignore = "Only the disposable native sealing test subprocess invokes this fixture"]
    fn sealed_runner_fixture() {
        assert_eq!(
            std::env::var("WW_SEALED_RUNNER_FIXTURE").as_deref(),
            Ok("1")
        );
        restricted_process_creation(true);
        println!("SEALED_RUNNER_FIXTURE_PASSED");
    }

    fn restricted_process_creation(seal: bool) {
        // This proves the restricted-token API path under the current account,
        // not administrator provisioning or the cross-account launcher path.
        let mut base = null_mut();
        assert_ne!(
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut base) },
            0
        );
        let base = unsafe { owned(base).unwrap() };
        let user = token_information(raw(&base), TokenUser).unwrap();
        let host = sid_text(unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid }).unwrap();
        let token = restricted_payload_token(&host).unwrap();
        if seal {
            let pid = unsafe { GetCurrentProcessId() };
            let tid = unsafe { GetCurrentThreadId() };
            let thread_fixture = || {
                let (send_id, receive_id) = std::sync::mpsc::channel();
                let (finish, receive_finish) = std::sync::mpsc::channel::<()>();
                let thread = std::thread::spawn(move || {
                    send_id.send(unsafe { GetCurrentThreadId() }).unwrap();
                    let _ = receive_finish.recv();
                });
                (receive_id.recv().unwrap(), finish, thread)
            };
            let before = thread_fixture();
            // Positive control proves the original same-account object grants
            // the dangerous access being tested. SYSTEM is the trusted host
            // stand-in, so this fixture's account receives only sandbox rights.
            drop(unsafe {
                owned(OpenProcess(PROCESS_VM_WRITE | PROCESS_DUP_HANDLE, 0, pid)).unwrap()
            });
            drop(unsafe {
                owned(OpenThread(
                    THREAD_SET_CONTEXT | THREAD_IMPERSONATE,
                    0,
                    before.0,
                ))
                .unwrap()
            });
            seal_current_runner("S-1-5-18", raw(&token)).unwrap();
            let after = thread_fixture();
            struct Revert;
            impl Drop for Revert {
                fn drop(&mut self) {
                    assert_ne!(unsafe { RevertToSelf() }, 0);
                }
            }
            assert_ne!(
                unsafe { ImpersonateLoggedOnUser(raw(&token)) },
                0,
                "{}",
                io::Error::last_os_error()
            );
            let revert = Revert;
            for access in [
                PROCESS_VM_WRITE,
                PROCESS_DUP_HANDLE,
                PROCESS_CREATE_THREAD,
                WRITE_DAC,
            ] {
                let denied = unsafe { OpenProcess(access, 0, pid) };
                assert!(
                    denied.is_null(),
                    "Runner process access unexpectedly granted: {access:#x}"
                );
                assert_eq!(unsafe { GetLastError() }, ERROR_ACCESS_DENIED);
            }
            drop(unsafe {
                owned(OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                    0,
                    pid,
                ))
                .unwrap()
            });
            for thread in [tid, before.0, after.0] {
                for access in [THREAD_SET_CONTEXT, THREAD_IMPERSONATE, WRITE_DAC] {
                    let denied = unsafe { OpenThread(access, 0, thread) };
                    assert!(
                        denied.is_null(),
                        "Runner thread {thread} access unexpectedly granted: {access:#x}"
                    );
                    assert_eq!(unsafe { GetLastError() }, ERROR_ACCESS_DENIED);
                }
            }
            let mut denied = null_mut();
            assert_eq!(
                unsafe {
                    OpenProcessToken(
                        GetCurrentProcess(),
                        TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY,
                        &mut denied,
                    )
                },
                0
            );
            assert_eq!(unsafe { GetLastError() }, ERROR_ACCESS_DENIED);
            drop(revert);
            for (_, finish, thread) in [before, after] {
                finish.send(()).unwrap();
                thread.join().unwrap();
            }
        }
        let job = unsafe { owned(CreateJobObjectW(null(), null())).unwrap() };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        assert_ne!(
            unsafe {
                SetInformationJobObject(
                    raw(&job),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            },
            0
        );
        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let nul = unsafe {
            owned(CreateFileW(
                wide("NUL").unwrap().as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &security,
                OPEN_EXISTING,
                0,
                null_mut(),
            ))
            .unwrap()
        };
        let inherited = [raw(&nul)];
        let jobs = [raw(&job)];
        let mut attributes = Attributes::with_capacity(3).unwrap();
        attributes
            .set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &inherited)
            .unwrap();
        attributes
            .set(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)
            .unwrap();
        attributes
            .set(
                PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
                &WIN32K_SYSTEM_CALL_DISABLE,
            )
            .unwrap();
        let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = raw(&nul);
        startup.StartupInfo.hStdOutput = raw(&nul);
        startup.StartupInfo.hStdError = raw(&nul);
        startup.lpAttributeList = attributes.pointer;
        let system_root = std::env::var("SystemRoot").unwrap();
        let exe = Path::new(&system_root).join("System32").join("cmd.exe");
        let mut command = quote_arg(exe.as_os_str());
        command.extend(" /d /c exit 23".encode_utf16());
        command.push(0);
        let env = environment(&BTreeMap::from([(
            "SystemRoot".into(),
            system_root.clone(),
        )]))
        .unwrap();
        let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
        let created = unsafe {
            CreateProcessAsUserW(
                raw(&token),
                wide(&exe).unwrap().as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_SUSPENDED
                    | DETACHED_PROCESS
                    | EXTENDED_STARTUPINFO_PRESENT
                    | CREATE_UNICODE_ENVIRONMENT,
                env.as_ptr().cast(),
                wide(system_root).unwrap().as_ptr(),
                &startup.StartupInfo,
                &mut info,
            )
        };
        assert_ne!(
            created,
            0,
            "{}",
            native_error("Native restricted CreateProcessAsUserW")
        );
        let mut payload = Payload {
            process: unsafe { owned(info.hProcess).unwrap() },
            thread: unsafe { owned(info.hThread).unwrap() },
            finished: false,
        };
        verify_payload_token(raw(&payload.process), raw(&token)).unwrap();
        let mut member = 0;
        assert_ne!(
            unsafe { IsProcessInJob(raw(&payload.process), raw(&job), &mut member) },
            0
        );
        assert_eq!(member, 1);
        assert_eq!(unsafe { ResumeThread(raw(&payload.thread)) }, 1);
        assert_eq!(
            unsafe { WaitForSingleObject(raw(&payload.process), 10_000) },
            WAIT_OBJECT_0
        );
        payload.finished = true;
        let mut exit = 0;
        assert_ne!(
            unsafe { GetExitCodeProcess(raw(&payload.process), &mut exit) },
            0
        );
        assert_eq!(exit, 23);
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        assert_ne!(
            unsafe {
                QueryInformationJobObject(
                    raw(&job),
                    JobObjectBasicAccountingInformation,
                    (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    null_mut(),
                )
            },
            0
        );
        assert_eq!(accounting.ActiveProcesses, 0);
    }
}
