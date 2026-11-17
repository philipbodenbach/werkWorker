//! Private noninteractive USER objects for one dedicated-account invocation.
//! The bootstrap creates and associates them; only the host hardens their ACLs
//! and retains independent handles until confirmed Job cleanup. Existing USER
//! objects and their ACLs are never changed.
use crate::core::{Result, WorkerError};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::OnceLock,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::DEVMODEW,
    Security::{Authorization::*, *},
    System::{
        LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
        StationsAndDesktops::{
            DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW, DESKTOP_READ_CONTROL, DESKTOP_READOBJECTS,
            DESKTOP_WRITEOBJECTS, HDESK, HWINSTA, UOI_FLAGS, UOI_NAME, UOI_TYPE,
            USER_OBJECT_INFORMATION_INDEX, USEROBJECTFLAGS,
        },
        SystemInformation::GetSystemDirectoryW,
        Threading::GetCurrentThreadId,
    },
};

// Static User32 imports make even unrelated single-process LPAC payloads load
// User32 before main, where their Win32k lockdown rejects initialization. Keep
// every desktop entry point lazy and restricted to the process-tree path.
type CreateDesktopEx = unsafe extern "system" fn(
    *const u16,
    *const u16,
    *const DEVMODEW,
    u32,
    u32,
    *const SECURITY_ATTRIBUTES,
    u32,
    *const c_void,
) -> HDESK;
type OpenDesktop = unsafe extern "system" fn(*const u16, u32, i32, u32) -> HDESK;
type CloseDesktop = unsafe extern "system" fn(HDESK) -> i32;
type GetThreadDesktop = unsafe extern "system" fn(u32) -> HDESK;
type SetThreadDesktop = unsafe extern "system" fn(HDESK) -> i32;
type GetProcessWindowStation = unsafe extern "system" fn() -> HWINSTA;
type CreateWindowStation =
    unsafe extern "system" fn(*const u16, u32, u32, *const SECURITY_ATTRIBUTES) -> HWINSTA;
type SetProcessWindowStation = unsafe extern "system" fn(HWINSTA) -> i32;
type CloseWindowStation = unsafe extern "system" fn(HWINSTA) -> i32;
type GetUserObjectInformation =
    unsafe extern "system" fn(HANDLE, i32, *mut c_void, u32, *mut u32) -> i32;

struct User32Api {
    // Deliberately retain the loader reference for process lifetime: desktop
    // handles and the pointers below must remain valid through supervisor cleanup.
    _module: usize,
    create_desktop: CreateDesktopEx,
    create_station: CreateWindowStation,
    set_station: SetProcessWindowStation,
    close_station: CloseWindowStation,
    open_desktop: OpenDesktop,
    close_desktop: CloseDesktop,
    get_thread_desktop: GetThreadDesktop,
    set_thread_desktop: SetThreadDesktop,
    get_process_window_station: GetProcessWindowStation,
    get_user_object_information: GetUserObjectInformation,
}
fn user32() -> Result<&'static User32Api> {
    static API: OnceLock<Result<User32Api>> = OnceLock::new();
    API.get_or_init(|| {
        let mut system = vec![0u16; 32768];
        let length = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), system.len() as u32) };
        if length == 0 || length as usize >= system.len() {
            return Err(failure("Resolve System32 for private desktop APIs"));
        }
        system.truncate(length as usize);
        system.extend("\\user32.dll".encode_utf16());
        system.push(0);
        let module =
            unsafe { LoadLibraryExW(system.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            return Err(failure("Load System32 private desktop APIs"));
        }
        macro_rules! resolve {
            ($name:literal, $ty:ty) => {{
                let address = unsafe { GetProcAddress(module, concat!($name, "\0").as_ptr()) }
                    .ok_or_else(|| failure(concat!("Resolve private desktop API ", $name)))?;
                // SAFETY: this exact System32 export has the declared Windows
                // ABI and signature. The module reference is never released.
                unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(address) }
            }};
        }
        Ok(User32Api {
            _module: module as usize,
            create_desktop: resolve!("CreateDesktopExW", CreateDesktopEx),
            create_station: resolve!("CreateWindowStationW", CreateWindowStation),
            set_station: resolve!("SetProcessWindowStation", SetProcessWindowStation),
            close_station: resolve!("CloseWindowStation", CloseWindowStation),
            open_desktop: resolve!("OpenDesktopW", OpenDesktop),
            close_desktop: resolve!("CloseDesktop", CloseDesktop),
            get_thread_desktop: resolve!("GetThreadDesktop", GetThreadDesktop),
            set_thread_desktop: resolve!("SetThreadDesktop", SetThreadDesktop),
            get_process_window_station: resolve!(
                "GetProcessWindowStation",
                GetProcessWindowStation
            ),
            get_user_object_information: resolve!(
                "GetUserObjectInformationW",
                GetUserObjectInformation
            ),
        })
    })
    .as_ref()
    .map_err(Clone::clone)
}

const SYSTEM_SID: &str = "S-1-5-18";
const DESKTOP_FULL: u32 = 0x000f_01ff;
const STATION_FULL: u32 = 0x000f_037f;
// The noninteractive station is private. Job UI restrictions separately forbid
// global atoms/desktop changes before untrusted code can run. Clipboard access
// is independently removed: native EmptyClipboard bypassed the Job write flag.
const STATION_SANDBOX: u32 = 0x0002_037b;
const DESKTOP_SANDBOX: u32 = DESKTOP_READ_CONTROL
    | DESKTOP_READOBJECTS
    | DESKTOP_CREATEWINDOW
    | DESKTOP_CREATEMENU
    | DESKTOP_WRITEOBJECTS;
const CWF_CREATE_ONLY: u32 = 1;

fn invalid(message: impl Into<String>) -> WorkerError {
    WorkerError::new("sandbox_unavailable", message)
}
fn failure(operation: &str) -> WorkerError {
    invalid(format!("{operation}: {}", std::io::Error::last_os_error()))
}
fn wide(value: &str) -> Result<Vec<u16>> {
    if value.contains('\0') {
        return Err(invalid("NUL in private USER object input"));
    }
    Ok(value.encode_utf16().chain(Some(0)).collect())
}
struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn sid_text(sid: PSID) -> Result<String> {
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(invalid("Invalid USER object principal"));
    }
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(failure("Format USER object principal"));
    }
    let _memory = Local(text.cast());
    let mut length = 0;
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
        String::from_utf16(std::slice::from_raw_parts(text, length))
            .map_err(|_| invalid("Invalid principal encoding"))
    }
}
fn canonical_sid(value: &str) -> Result<()> {
    let mut sid = null_mut();
    if unsafe { ConvertStringSidToSidW(wide(value)?.as_ptr(), &mut sid) } == 0 {
        return Err(failure("Parse USER object principal"));
    }
    let _sid = Local(sid);
    if sid_text(sid)? != value {
        return Err(invalid("Noncanonical USER object principal"));
    }
    Ok(())
}
fn principals(host: &str, user: &str, full: u32, limited: u32) -> Result<BTreeMap<String, u32>> {
    canonical_sid(host)?;
    canonical_sid(user)?;
    if host == user || host == SYSTEM_SID || user == SYSTEM_SID {
        return Err(invalid("USER object principals are not distinct"));
    }
    Ok(BTreeMap::from([
        (SYSTEM_SID.to_owned(), full),
        (host.to_owned(), full),
        (user.to_owned(), limited),
    ]))
}
fn descriptor(owner: &str, host: &str, user: &str, full: u32, limited: u32) -> Result<Local> {
    let _ = principals(host, user, full, limited)?;
    let text = wide(&format!(
        "O:{owner}D:P(A;;0x{full:x};;;SY)(A;;0x{full:x};;;{host})(A;;0x{limited:x};;;{user})"
    ))?;
    let mut sd = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(), 1, &mut sd, null_mut())
    } == 0
    {
        return Err(failure("Create private USER object descriptor"));
    }
    Ok(Local(sd))
}
fn object_text(handle: HANDLE, index: USER_OBJECT_INFORMATION_INDEX) -> Result<String> {
    let api = user32()?;
    let mut needed = 0;
    unsafe {
        (api.get_user_object_information)(handle, index, null_mut(), 0, &mut needed);
    }
    if !(2..=65536).contains(&needed) || needed % 2 != 0 {
        return Err(failure("Size USER object information"));
    }
    let mut text = vec![0u16; needed as usize / 2];
    if unsafe {
        (api.get_user_object_information)(
            handle,
            index,
            text.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(failure("Read USER object information"));
    }
    let length = text
        .iter()
        .position(|v| *v == 0)
        .ok_or_else(|| invalid("Unterminated USER object name"))?;
    String::from_utf16(&text[..length]).map_err(|_| invalid("Invalid USER object name encoding"))
}
fn validate_nonce(nonce: &str) -> Result<()> {
    let uuid = nonce
        .strip_prefix("werk-tree-")
        .ok_or_else(|| invalid("Invalid private desktop namespace"))?;
    if uuid.len() != 36 || uuid::Uuid::parse_str(uuid).is_err() {
        return Err(invalid("Invalid private desktop nonce"));
    }
    Ok(())
}
fn split_qualified(qualified: &str) -> Result<(&str, &str)> {
    let (station, desktop) = qualified
        .split_once('\\')
        .ok_or_else(|| invalid("Private desktop lacks a station"))?;
    if station.is_empty() || station.len() > 256 || station.contains('\0') {
        return Err(invalid("Invalid private station name"));
    }
    validate_nonce(desktop)?;
    Ok((station, desktop))
}
fn valid_handle(handle: HANDLE) -> Result<()> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(invalid("Missing retained USER object handle"));
    }
    Ok(())
}
fn verify_object(handle: HANDLE, name: &str, kind: &str) -> Result<()> {
    valid_handle(handle)?;
    if object_text(handle, UOI_TYPE)? != kind || object_text(handle, UOI_NAME)? != name {
        return Err(invalid("Retained USER object identity changed"));
    }
    let mut flags: USEROBJECTFLAGS = unsafe { zeroed() };
    if unsafe {
        (user32()?.get_user_object_information)(
            handle,
            UOI_FLAGS,
            (&mut flags as *mut USEROBJECTFLAGS).cast(),
            size_of::<USEROBJECTFLAGS>() as u32,
            null_mut(),
        )
    } == 0
    {
        return Err(failure("Read private USER object flags"));
    }
    // For stations, WSF_VISIBLE must be absent. For desktops this also rejects
    // DF_ALLOWOTHERACCOUNTHOOK; neither object handle may be inheritable.
    if flags.fInherit != 0 || flags.dwFlags != 0 {
        return Err(invalid("Private USER object is interactive or inheritable"));
    }
    Ok(())
}
fn verify_security(
    handle: HANDLE,
    owner_expected: &str,
    host: &str,
    user: &str,
    full: u32,
    limited: u32,
) -> Result<()> {
    let mut expected = principals(host, user, full, limited)?;
    let mut owner = null_mut();
    let mut acl = null_mut();
    let mut sd = null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_WINDOW_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut acl,
            null_mut(),
            &mut sd,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(invalid(format!(
            "Read private USER object ACL: Windows error {status}"
        )));
    }
    let _sd = Local(sd);
    let mut control = 0;
    let mut revision = 0;
    if unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) } == 0 {
        return Err(failure("Read USER object ACL protection"));
    }
    if sid_text(owner)? != owner_expected
        || control & SE_DACL_PROTECTED == 0
        || acl.is_null()
        || unsafe { IsValidAcl(acl) } == 0
        || unsafe { (*acl).AceCount } as usize != expected.len()
    {
        return Err(invalid(
            "Private USER object owner or protected ACL changed",
        ));
    }
    for index in 0..u32::from(unsafe { (*acl).AceCount }) {
        let mut ace = null_mut();
        if unsafe { GetAce(acl, index, &mut ace) } == 0 {
            return Err(failure("Read USER object ACE"));
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if header.AceType != 0
            || header.AceFlags != 0
            || (header.AceSize as usize) < size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Err(invalid("Unexpected USER object ACE"));
        }
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = sid_text(std::ptr::addr_of!(allowed.SidStart).cast_mut().cast())?;
        if expected.remove(&sid) != Some(allowed.Mask) {
            return Err(invalid("USER object access differs from exact policy"));
        }
    }
    if !expected.is_empty() {
        return Err(invalid("Incomplete USER object ACL"));
    }
    Ok(())
}
fn verify_pair(
    station: HANDLE,
    desktop: HANDLE,
    qualified: &str,
    host: &str,
    user: &str,
    hardened: bool,
) -> Result<()> {
    let (station_name, desktop_name) = split_qualified(qualified)?;
    verify_object(station, station_name, "WindowStation")?;
    verify_object(desktop, desktop_name, "Desktop")?;
    let owner = if hardened { host } else { user };
    verify_security(
        station,
        owner,
        host,
        user,
        STATION_FULL,
        if hardened {
            STATION_SANDBOX
        } else {
            STATION_FULL
        },
    )?;
    verify_security(
        desktop,
        owner,
        host,
        user,
        DESKTOP_FULL,
        if hardened {
            DESKTOP_SANDBOX
        } else {
            DESKTOP_FULL
        },
    )
}

/// Checks current association without changing the calling process or thread.
/// The Job UI restrictions must already have been verified by the host.
pub(super) fn verify_current(qualified: &str) -> Result<()> {
    let (station, desktop) = split_qualified(qualified)?;
    let api = user32()?;
    verify_object(
        unsafe { (api.get_process_window_station)() },
        station,
        "WindowStation",
    )?;
    verify_object(
        unsafe { (api.get_thread_desktop)(GetCurrentThreadId()) },
        desktop,
        "Desktop",
    )
}

/// Independent duplicates held only by the host's cleanup supervisor. Unlike
/// bootstrap handles, these are never assigned to a host process or thread.
pub(super) struct HostResources {
    station: HWINSTA,
    desktop: HDESK,
}
// SAFETY: only detached host duplicates enter this owner. The host never assigns
// them to itself; shared queries do not mutate them and closing needs &mut self.
unsafe impl Send for HostResources {}
unsafe impl Sync for HostResources {}
impl HostResources {
    /// Takes ownership before validation, including partial handoffs. Each
    /// nonnull handle must be an owned, noninheritable duplicate of its type.
    pub(super) unsafe fn from_raw(station: HANDLE, desktop: HANDLE) -> Self {
        Self { station, desktop }
    }
    /// The station slot must be empty; ownership of this duplicate moves here.
    pub(super) unsafe fn adopt_station(&mut self, handle: HANDLE) {
        debug_assert!(self.station.is_null());
        self.station = handle;
    }
    /// The desktop slot must be empty; ownership of this duplicate moves here.
    pub(super) unsafe fn adopt_desktop(&mut self, handle: HANDLE) {
        debug_assert!(self.desktop.is_null());
        self.desktop = handle;
    }
    pub(super) fn verify(&self, qualified: &str, host: &str, user: &str) -> Result<()> {
        verify_pair(self.station, self.desktop, qualified, host, user, true)
    }
    /// The authenticated bootstrap must have freshly created these objects.
    /// Exact creation ACLs and names are checked before either ACL is changed.
    pub(super) fn verify_and_harden(&self, qualified: &str, host: &str, user: &str) -> Result<()> {
        verify_pair(self.station, self.desktop, qualified, host, user, false)?;
        for (handle, full, limited) in [
            (self.station, STATION_FULL, STATION_SANDBOX),
            (self.desktop, DESKTOP_FULL, DESKTOP_SANDBOX),
        ] {
            let sd = descriptor(host, host, user, full, limited)?;
            let mut owner = null_mut();
            let mut acl = null_mut();
            let mut defaulted = 0;
            let mut present = 0;
            if unsafe { GetSecurityDescriptorOwner(sd.0, &mut owner, &mut defaulted) } == 0
                || unsafe {
                    GetSecurityDescriptorDacl(sd.0, &mut present, &mut acl, &mut defaulted)
                } == 0
                || present == 0
                || acl.is_null()
            {
                return Err(invalid("Invalid private USER hardening descriptor"));
            }
            let status = unsafe {
                SetSecurityInfo(
                    handle,
                    SE_WINDOW_OBJECT,
                    OWNER_SECURITY_INFORMATION
                        | DACL_SECURITY_INFORMATION
                        | PROTECTED_DACL_SECURITY_INFORMATION,
                    owner,
                    null_mut(),
                    acl,
                    null_mut(),
                )
            };
            if status != ERROR_SUCCESS {
                return Err(invalid(format!(
                    "Harden newly created USER object: Windows error {status}"
                )));
            }
        }
        self.verify(qualified, host, user)
    }
    /// The host calls this only after confirmed Job/process cleanup, while its
    /// workspace lease remains held. Failure retains the handle for retry.
    pub(super) fn close(&mut self) -> Result<()> {
        if !self.desktop.is_null() {
            if unsafe { (user32()?.close_desktop)(self.desktop) } == 0 {
                return Err(failure("Close retained private desktop"));
            }
            self.desktop = null_mut();
        }
        if !self.station.is_null() {
            if unsafe { (user32()?.close_station)(self.station) } == 0 {
                return Err(failure("Close retained private station"));
            }
            self.station = null_mut();
        }
        Ok(())
    }
}
impl Drop for HostResources {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct CreationGuard {
    owned: HostResources,
    original_station: HWINSTA,
    original_desktop: HDESK,
}
impl CreationGuard {
    fn cleanup(&mut self) -> Result<()> {
        let api = user32()?;
        if unsafe { (api.get_process_window_station)() } != self.original_station
            && unsafe { (api.set_station)(self.original_station) } == 0
        {
            return Err(failure("Restore failed bootstrap station creation"));
        }
        if unsafe { (api.get_thread_desktop)(GetCurrentThreadId()) } != self.original_desktop
            && unsafe { (api.set_thread_desktop)(self.original_desktop) } == 0
        {
            return Err(failure("Restore failed bootstrap desktop creation"));
        }
        if unsafe { (api.get_process_window_station)() } != self.original_station
            || unsafe { (api.get_thread_desktop)(GetCurrentThreadId()) } != self.original_desktop
        {
            return Err(invalid("Bootstrap USER restore was not confirmed"));
        }
        self.owned.close()
    }
}
impl Drop for CreationGuard {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Used only by the trusted dedicated-account bootstrap, before any Job UI
/// restrictions or payload execution. Successful creation deliberately retains
/// the associated handles for process lifetime; the host owns other duplicates.
/// Every error must terminate bootstrap startup without running a payload.
pub(super) struct BootstrapResources {
    station: HWINSTA,
    desktop: HDESK,
}
impl BootstrapResources {
    pub(super) fn create(nonce: &str, host: &str, user: &str) -> Result<Self> {
        validate_nonce(nonce)?;
        let _ = principals(host, user, STATION_FULL, STATION_FULL)?;
        let api = user32()?;
        let original_station = unsafe { (api.get_process_window_station)() };
        let original_desktop = unsafe { (api.get_thread_desktop)(GetCurrentThreadId()) };
        valid_handle(original_station)?;
        valid_handle(original_desktop)?;
        let mut guard = CreationGuard {
            owned: unsafe { HostResources::from_raw(null_mut(), null_mut()) },
            original_station,
            original_desktop,
        };
        let result = (|| -> Result<()> {
            let sd = descriptor(user, host, user, STATION_FULL, STATION_FULL)?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: 0,
            };
            unsafe {
                SetLastError(ERROR_SUCCESS);
            }
            let station =
                unsafe { (api.create_station)(null(), CWF_CREATE_ONLY, STATION_FULL, &sa) };
            let error = unsafe { GetLastError() };
            unsafe {
                guard.owned.adopt_station(station);
            }
            if station.is_null() {
                return Err(invalid(format!(
                    "Create fresh dedicated window station: Windows error {error}"
                )));
            }
            if error == ERROR_ALREADY_EXISTS {
                return Err(invalid("Fresh window station collided"));
            }
            if unsafe { (api.set_station)(station) } == 0 {
                return Err(failure("Associate bootstrap private window station"));
            }
            let name = wide(nonce)?;
            let existing = unsafe { (api.open_desktop)(name.as_ptr(), 0, 0, DESKTOP_FULL) };
            if !existing.is_null() {
                unsafe {
                    (api.close_desktop)(existing);
                }
                return Err(invalid("Fresh desktop name already exists"));
            }
            if unsafe { GetLastError() } != ERROR_FILE_NOT_FOUND {
                return Err(failure("Check fresh desktop name"));
            }
            let sd = descriptor(user, host, user, DESKTOP_FULL, DESKTOP_FULL)?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: 0,
            };
            unsafe {
                SetLastError(ERROR_SUCCESS);
            }
            let desktop = unsafe {
                (api.create_desktop)(
                    name.as_ptr(),
                    null(),
                    null(),
                    0,
                    DESKTOP_FULL,
                    &sa,
                    0,
                    null(),
                )
            };
            let error = unsafe { GetLastError() };
            unsafe {
                guard.owned.adopt_desktop(desktop);
            }
            if desktop.is_null() {
                return Err(invalid(format!(
                    "Create fresh dedicated desktop: Windows error {error}"
                )));
            }
            if error == ERROR_ALREADY_EXISTS {
                return Err(invalid("Fresh dedicated desktop collided"));
            }
            if unsafe { (api.set_thread_desktop)(desktop) } == 0 {
                return Err(failure("Associate bootstrap private desktop"));
            }
            let qualified = format!("{}\\{nonce}", object_text(station, UOI_NAME)?);
            verify_pair(station, desktop, &qualified, host, user, false)?;
            verify_current(&qualified)
        })();
        if let Err(error) = result {
            return match guard.cleanup() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(invalid(format!(
                    "{error}; bootstrap USER cleanup pending process termination: {cleanup}"
                ))),
            };
        }
        let resources = Self {
            station: guard.owned.station,
            desktop: guard.owned.desktop,
        };
        // Success keeps this bootstrap associated. Its process exit closes its
        // USER handles; the host's independent copies control final object life.
        guard.owned.station = null_mut();
        guard.owned.desktop = null_mut();
        std::mem::forget(guard);
        Ok(resources)
    }
    pub(super) fn station_name(&self) -> Result<String> {
        object_text(self.station, UOI_NAME)
    }
    pub(super) fn qualified_name(&self) -> Result<String> {
        Ok(format!(
            "{}\\{}",
            self.station_name()?,
            object_text(self.desktop, UOI_NAME)?
        ))
    }
    pub(super) fn station_raw(&self) -> HANDLE {
        self.station
    }
    pub(super) fn desktop_raw(&self) -> HANDLE {
        self.desktop
    }
    pub(super) fn verify_hardened(&self, qualified: &str, host: &str, user: &str) -> Result<()> {
        verify_current(qualified)?;
        verify_pair(self.station, self.desktop, qualified, host, user, true)
    }
}
