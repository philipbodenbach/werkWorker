//! Provision-specific package identity, write-restricting local group and profile storage.
use super::*;
use std::{
    ffi::{OsStr, OsString},
    os::windows::{
        ffi::OsStringExt,
        io::{FromRawHandle, OwnedHandle},
    },
};
use windows_sys::Win32::{
    Security::Isolation::*,
    System::{
        Com::CoTaskMemFree,
        Registry::*,
        SystemInformation::GetSystemDirectoryW,
        SystemServices::{
            MAXIMUM_ALLOWED, SYSTEM_MANDATORY_LABEL_ACE_TYPE, SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
        },
    },
};

const WORLD: &str = "S-1-1-0";

fn hresult(op: &str, hr: i32) -> Result<()> {
    if hr == 0 {
        Ok(())
    } else {
        Err(fail(format!("{op}: HRESULT {hr:#x}")))
    }
}
fn moniker(user: &str) -> Result<String> {
    let parsed = parse_sid(user)?;
    if sid_text(parsed.0)? != user {
        return Err(fail("Noncanonical sandbox SID"));
    }
    let hash = format!("{:x}", Sha256::digest(user.as_bytes()));
    Ok(format!("werkworker.tree.{}", &hash[..32]))
}
pub(super) fn package_sid(user: &str) -> Result<String> {
    let name = wide(moniker(user)?)?;
    let mut sid = null_mut();
    hresult("Derive sandbox package identity", unsafe {
        DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid)
    })?;
    let result = sid_text(sid);
    unsafe {
        FreeSid(sid);
    }
    result
}
fn writer_name(username: &str) -> Result<String> {
    let suffix = username
        .strip_prefix("wwbox_")
        .filter(|s| s.len() == 12 && s.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| fail("Invalid dedicated sandbox name"))?;
    Ok(format!("wwrite_{suffix}"))
}
pub(super) struct WriterGroup {
    name: String,
    pub(super) sid: String,
}
impl WriterGroup {
    pub(super) fn create(username: &str) -> Result<Self> {
        let name = writer_name(username)?;
        let mut wide_name = wide(&name)?;
        let info = LOCALGROUP_INFO_0 {
            lgrpi0_name: wide_name.as_mut_ptr(),
        };
        status("Create empty sandbox write group", unsafe {
            NetLocalGroupAdd(
                null(),
                0,
                (&info as *const LOCALGROUP_INFO_0).cast(),
                null_mut(),
            )
        })?;
        let sid = match account_sid(&name) {
            Ok(s) => s,
            Err(e) => {
                return Err(fail(format!(
                    "Cannot bind newly created write group {name}: {e}"
                )));
            }
        };
        let group = Self { name, sid };
        if let Err(e) = verify_writer(username, &group.sid) {
            let cleanup = group.rollback();
            return Err(fail(format!(
                "Cannot bind write group: {e}; rollback {cleanup:?}"
            )));
        }
        Ok(group)
    }
    pub(super) fn rollback(&self) -> Result<()> {
        if account_sid(&self.name)? != self.sid {
            return Err(fail("Write group identity changed during rollback"));
        }
        status("Remove newly created write group", unsafe {
            NetLocalGroupDel(null(), wide(&self.name)?.as_ptr())
        })
    }
}
pub(super) fn verify_writer(username: &str, sid: &str) -> Result<()> {
    let name = writer_name(username)?;
    if account_sid(&name)? != sid {
        return Err(fail("Sandbox write group identity changed"));
    }
    let mut buffer = null_mut();
    let (mut count, mut total) = (0, 0);
    let result = unsafe {
        NetLocalGroupGetMembers(
            null(),
            wide(name)?.as_ptr(),
            0,
            &mut buffer,
            u32::MAX,
            &mut count,
            &mut total,
            null_mut(),
        )
    };
    if !buffer.is_null() {
        unsafe {
            NetApiBufferFree(buffer.cast());
        }
    }
    status("Inspect write restriction group", result)?;
    if count != 0 || total != 0 {
        return Err(fail("Sandbox write group must remain empty"));
    }
    Ok(())
}
pub(super) fn verify_low_directory(handle: HANDLE) -> Result<()> {
    let (mut sacl, mut sd) = (null_mut(), null_mut());
    status("Read workspace integrity label", unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sacl,
            &mut sd,
        )
    })?;
    let _sd = Local(sd);
    if sacl.is_null() || unsafe { (*sacl).AceCount } != 1 {
        return Err(fail("Expected one inheritable Low integrity label"));
    }
    let mut ace = null_mut();
    if unsafe { GetAce(sacl, 0, &mut ace) } == 0 {
        return Err(last("Read Low integrity ACE"));
    }
    let ace = unsafe { &*ace.cast::<SYSTEM_MANDATORY_LABEL_ACE>() };
    let sid = std::ptr::addr_of!(ace.SidStart).cast_mut().cast();
    if ace.Header.AceType != SYSTEM_MANDATORY_LABEL_ACE_TYPE as u8
        || ace.Header.AceFlags != (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8
        || ace.Mask != SYSTEM_MANDATORY_LABEL_NO_WRITE_UP
        || sid_text(sid)? != "S-1-16-4096"
    {
        return Err(fail(
            "Workspace lost its inheritable Low integrity boundary",
        ));
    }
    Ok(())
}
fn system_directory() -> Result<PathBuf> {
    let mut path = vec![0; 32768];
    let count = unsafe { GetSystemDirectoryW(path.as_mut_ptr(), path.len() as u32) } as usize;
    if count == 0 || count >= path.len() {
        return Err(last("Resolve trusted system directory"));
    }
    Ok(PathBuf::from(OsString::from_wide(&path[..count])))
}
fn take_path(raw: *mut u16) -> Result<PathBuf> {
    if raw.is_null() {
        return Err(fail("Missing native package path"));
    }
    let mut n = 0;
    while n < 32768 && unsafe { *raw.add(n) } != 0 {
        n += 1;
    }
    let path = PathBuf::from(OsString::from_wide(unsafe {
        std::slice::from_raw_parts(raw, n)
    }));
    unsafe {
        CoTaskMemFree(raw.cast());
    }
    if n == 32768 || !path.is_absolute() {
        return Err(fail("Invalid native package path"));
    }
    Ok(path)
}
fn local_packages() -> Result<PathBuf> {
    Ok(super::super::windows::local_app_data()?.join("Packages"))
}
/// Resolve the directory from an OS-issued user token and the host-pinned, provisioned temp.
/// This does not consult the desktop shell or inherit LocalAppData from the launcher.
fn bound_local_app_data(
    token: HANDLE,
    user: &str,
    package: &str,
    expected_temp: &Path,
) -> Result<PathBuf> {
    if token_user_sid(token)? != user || package_sid(user)? != package {
        return Err(fail("Dedicated profile token or package identity differs"));
    }
    let mut buffer = vec![0u16; 32768];
    let mut n = buffer.len() as u32;
    if unsafe {
        windows_sys::Win32::UI::Shell::GetUserProfileDirectoryW(token, buffer.as_mut_ptr(), &mut n)
    } == 0
    {
        return Err(last("Read the dedicated token's Windows profile directory"));
    }
    let n = buffer
        .iter()
        .position(|c| *c == 0)
        .ok_or_else(|| fail("Invalid Windows user profile path"))?;
    let root = PathBuf::from(OsString::from_wide(&buffer[..n]));
    if !root.is_absolute() || !expected_temp.is_absolute() {
        return Err(fail(
            "Absolute dedicated user profile and temp paths are required",
        ));
    }
    let local = root.join("AppData").join("Local");
    let temp = local
        .join("Packages")
        .join(moniker(user)?)
        .join("AC")
        .join("Temp");
    let mut pins = Vec::new();
    for path in temp.ancestors().take_while(|path| path.starts_with(&root)) {
        pins.push(open_profile_dir(
            path,
            FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
        )?);
    }
    let approved = open_profile_dir(expected_temp, FILE_READ_ATTRIBUTES | FILE_TRAVERSE)?;
    if std::fs::canonicalize(expected_temp)? != std::fs::canonicalize(&temp)?
        || file_identity(&approved)? != file_identity(&pins[0])?
    {
        return Err(fail(
            "OS profile temp differs from the host-pinned package temp",
        ));
    }
    Ok(local)
}
pub(crate) fn verified_runner_local_app_data(
    user: &str,
    package: &str,
    expected_temp: &Path,
) -> Result<PathBuf> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last("Open dedicated runner profile token"));
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    bound_local_app_data(token.as_raw_handle(), user, package, expected_temp)
}
fn package_directory(user: &str, package: &str) -> Result<PathBuf> {
    let expected = local_packages()?.join(moniker(user)?);
    let mut path = null_mut();
    hresult("Resolve registered package storage", unsafe {
        GetAppContainerFolderPath(wide(package)?.as_ptr(), &mut path)
    })?;
    let actual = take_path(path)?;
    let root = actual
        .parent()
        .ok_or_else(|| fail("Package storage has no parent"))?;
    if actual.file_name() != Some(OsStr::new("AC"))
        || std::fs::canonicalize(root)? != std::fs::canonicalize(&expected)?
    {
        return Err(fail(
            "Registered package points outside its dedicated profile",
        ));
    }
    Ok(root.to_owned())
}
fn impersonate<T>(token: HANDLE, action: impl FnOnce() -> Result<T>) -> Result<T> {
    struct Revert;
    impl Drop for Revert {
        fn drop(&mut self) {
            if unsafe { RevertToSelf() } == 0 {
                unsafe { ExitProcess(125) }
            }
        }
    }
    if unsafe { ImpersonateLoggedOnUser(token) } == 0 {
        return Err(last("Inspect effective package storage"));
    }
    let _revert = Revert;
    action()
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
fn registry_root(token: HANDLE, user: &str, setup: bool) -> Result<Key> {
    use crate::runtime::windows_tree_package_namespace::object_name;
    let suffix = format!(
        r"{}_Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Storage\{}",
        user,
        moniker(user)?
    );
    let expected = format!(r"\REGISTRY\USER\{suffix}");
    // The OS may initialize storage on first use. Obtain its own independent
    // handle at setup and prove this account/package layout before any edits.
    let registered = if setup {
        Some(impersonate(token, || {
            let mut key = null_mut();
            hresult("Open freshly registered OS package key", unsafe {
                GetAppContainerRegistryLocation(KEY_READ, &mut key)
            })?;
            Ok(Key(key))
        })?)
    } else {
        None
    };
    let mut pinned = null_mut();
    status("Pin the dedicated user's registered package key", unsafe {
        RegOpenKeyExW(
            HKEY_USERS,
            wide(&suffix)?.as_ptr(),
            REG_OPTION_OPEN_LINK,
            KEY_READ | if setup { WRITE_DAC } else { 0 },
            &mut pinned,
        )
    })?;
    let pinned = Key(pinned);
    let pinned_name = object_name(pinned.0)?;
    if !pinned_name.eq_ignore_ascii_case(&expected) {
        return Err(fail(
            "Trusted package registry handle has an unexpected kernel name",
        ));
    }
    if let Some(storage) = registered
        && !object_name(storage.0)?.eq_ignore_ascii_case(&pinned_name)
    {
        return Err(fail(
            "OS-created package registry key differs from the provisioned binding",
        ));
    }
    Ok(pinned)
}
fn checked_deny(handle: HANDLE, kind: SE_OBJECT_TYPE, mask: u32, setup: bool) -> Result<()> {
    let (mut dacl, mut sd) = (null_mut(), null_mut());
    status("Inspect private package DACL", unsafe {
        GetSecurityInfo(
            handle,
            kind,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    })?;
    let _sd = Local(sd);
    if dacl.is_null() {
        return Err(fail("Package storage lacks a DACL"));
    }
    let world = parse_sid(WORLD)?;
    let count = unsafe { (*dacl).AceCount } as u32;
    if count > 0 {
        let mut p = null_mut();
        if unsafe { GetAce(dacl, 0, &mut p) } == 0 {
            return Err(last("Inspect first package ACE"));
        }
        let ace = unsafe { &*p.cast::<ACCESS_DENIED_ACE>() };
        if ace.Header.AceType == 1
            && ace.Header.AceFlags == 0
            && ace.Mask == mask
            && unsafe { EqualSid(world.0, std::ptr::addr_of!(ace.SidStart).cast_mut().cast()) } != 0
        {
            return Ok(());
        }
    }
    if !setup {
        return Err(fail(
            "Package storage mutation barrier is missing or changed",
        ));
    }
    let bytes = (unsafe { (*dacl).AclSize } as usize)
        + size_of::<ACCESS_DENIED_ACE>()
        + unsafe { GetLengthSid(world.0) } as usize;
    if bytes > u16::MAX as usize {
        return Err(fail("Oversized package storage ACL"));
    }
    let mut words = vec![0u32; bytes.div_ceil(4)];
    let acl = words.as_mut_ptr().cast::<ACL>();
    if unsafe { InitializeAcl(acl, bytes as u32, ACL_REVISION_DS) } == 0
        || unsafe { AddAccessDeniedAceEx(acl, ACL_REVISION_DS, 0, mask, world.0) } == 0
    {
        return Err(last("Build private package mutation barrier"));
    }
    for i in 0..count {
        let mut p = null_mut();
        if unsafe { GetAce(dacl, i, &mut p) } == 0 {
            return Err(last("Inspect prior package ACE"));
        }
        let size = unsafe { (*p.cast::<ACE_HEADER>()).AceSize };
        if unsafe { AddAce(acl, ACL_REVISION_DS, u32::MAX, p, size.into()) } == 0 {
            return Err(last("Preserve prior package ACE"));
        }
    }
    status("Seal newly created package storage", unsafe {
        SetSecurityInfo(
            handle,
            kind,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl,
            null(),
        )
    })?;
    checked_deny(handle, kind, mask, false)
}

// PUBLIC_OBJECT_BASIC_INFORMATION (winternl.h) has a fixed 56-byte ABI.
#[repr(C)]
struct ObjectBasicInformation {
    attributes: u32,
    granted: u32,
    handle_count: u32,
    pointer_count: u32,
    reserved: [u32; 10],
}
fn verify_read_only(handle: HANDLE, mutations: u32) -> Result<()> {
    let mut info: ObjectBasicInformation = unsafe { zeroed() };
    let mut len = 0;
    let query = crate::runtime::windows_tree_package_namespace::nt_query_object()?;
    let status = unsafe {
        query(
            handle,
            0,
            (&mut info as *mut ObjectBasicInformation).cast(),
            size_of::<ObjectBasicInformation>() as u32,
            &mut len,
        )
    };
    if status != 0 || info.granted & mutations != 0 {
        return Err(fail(format!(
            "Private package storage has unverified or mutating access (status {status:#x}, granted {:#x})",
            info.granted
        )));
    }
    Ok(())
}
fn check_file_access(token: HANDLE, path: &Path, pinned: &File) -> Result<()> {
    impersonate(token, || {
        let file = match OpenOptions::new()
            .access_mode(MAXIMUM_ALLOWED)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let identity = |f: &File| -> Result<(u64, [u8; 16])> {
            let mut id: FILE_ID_INFO = unsafe { zeroed() };
            if unsafe {
                GetFileInformationByHandleEx(
                    f.as_raw_handle(),
                    FileIdInfo,
                    (&mut id as *mut FILE_ID_INFO).cast(),
                    size_of::<FILE_ID_INFO>() as u32,
                )
            } == 0
            {
                return Err(last("Verify pinned package storage identity"));
            }
            Ok((id.VolumeSerialNumber, id.FileId.Identifier))
        };
        if identity(&file)? != identity(pinned)? {
            return Err(fail("Package storage object was replaced"));
        }
        verify_read_only(
            file.as_raw_handle(),
            FILE_WRITE_DATA
                | FILE_APPEND_DATA
                | FILE_WRITE_EA
                | FILE_WRITE_ATTRIBUTES
                | FILE_DELETE_CHILD
                | DELETE
                | WRITE_DAC
                | WRITE_OWNER,
        )
    })
}
fn check_key_access(token: HANDLE, key: &Key) -> Result<()> {
    let mutations =
        KEY_SET_VALUE | KEY_CREATE_SUB_KEY | KEY_CREATE_LINK | DELETE | WRITE_DAC | WRITE_OWNER;
    let mapping = GENERIC_MAPPING {
        GenericRead: KEY_READ,
        GenericWrite: KEY_WRITE,
        GenericExecute: KEY_EXECUTE,
        GenericAll: KEY_ALL_ACCESS,
    };
    let effective = effective_access(token, key.0, SE_REGISTRY_KEY, &mapping)?;
    if effective & mutations != 0 {
        return Err(fail(format!(
            "Unexpected effective registry write rights: {effective:#x}"
        )));
    }
    impersonate(token, || {
        let mut actual = null_mut();
        let result = unsafe { RegOpenKeyExW(key.0, null(), 0, MAXIMUM_ALLOWED, &mut actual) };
        if result == ERROR_ACCESS_DENIED {
            return Ok(());
        }
        status("Inspect actual package registry access", result)?;
        let actual = Key(actual);
        verify_read_only(actual.0, mutations)
    })
}
fn check_storage(
    token: HANDLE,
    user: &str,
    package: &str,
    expected_temp: Option<&Path>,
) -> Result<()> {
    let setup = expected_temp.is_none();
    if package != package_sid(user)? {
        return Err(fail("Package identity changed"));
    }
    let root = if let Some(expected) = expected_temp {
        bound_local_app_data(token, user, package, expected)?
            .join("Packages")
            .join(moniker(user)?)
    } else {
        package_directory(user, package)?
    };
    let private_temp = root.join("AC").join("Temp");
    if let Some(expected) = expected_temp
        && std::fs::canonicalize(expected)? != std::fs::canonicalize(&private_temp)?
    {
        return Err(fail(
            "Host-approved temporary directory differs from the Windows package temp",
        ));
    }
    let mut stack = vec![root];
    let mut files = Vec::new();
    while let Some(path) = stack.pop() {
        if files.len() + stack.len() > 512 {
            return Err(fail("Unbounded package storage tree"));
        }
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL | if setup { WRITE_DAC } else { 0 })
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)?;
        let meta = file.metadata()?;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(fail("Unexpected reparse point in package storage"));
        }
        if path == private_temp {
            if !meta.is_dir() {
                return Err(fail("Windows package temp is not a directory"));
            }
            if setup && std::fs::read_dir(&path)?.next().is_some() {
                return Err(fail("Fresh package temp must be empty"));
            }
            if !setup {
                verify_temp_access(token, &path, &file)?;
            }
            continue;
        }
        if meta.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        } else {
            let mut info = unsafe { zeroed() };
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0
                || info.nNumberOfLinks != 1
            {
                return Err(fail("Unexpected file link in package storage"));
            }
        }
        files.push((path, file, meta.is_dir()));
    }
    // Pin and enumerate the complete fresh subtree before setting its first descriptor.
    let mut keys = vec![registry_root(token, user, setup)?];
    let mut i = 0;
    while i < keys.len() {
        let parent = keys[i].0;
        let (mut ty, mut len) = (0, 0);
        let rc = unsafe {
            RegQueryValueExW(
                parent,
                wide("SymbolicLinkValue")?.as_ptr(),
                null_mut(),
                &mut ty,
                null_mut(),
                &mut len,
            )
        };
        if rc != ERROR_FILE_NOT_FOUND && (rc != 0 || ty == REG_LINK) {
            return Err(fail(
                "Registry links are forbidden in private package storage",
            ));
        }
        let mut index = 0;
        loop {
            let mut name = [0u16; 256];
            let mut n = name.len() as u32;
            let rc = unsafe {
                RegEnumKeyExW(
                    parent,
                    index,
                    name.as_mut_ptr(),
                    &mut n,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            status("Enumerate private package registry", rc)?;
            if n == 0 || n >= name.len() as u32 || keys.len() >= 256 {
                return Err(fail("Unbounded package registry tree"));
            }
            name[n as usize] = 0;
            let mut key = null_mut();
            status("Open private package subkey", unsafe {
                RegOpenKeyExW(
                    parent,
                    name.as_ptr(),
                    REG_OPTION_OPEN_LINK,
                    KEY_READ | if setup { WRITE_DAC } else { 0 },
                    &mut key,
                )
            })?;
            keys.push(Key(key));
            index += 1;
        }
        i += 1;
    }
    for (path, file, directory) in &files {
        if *directory {
            checked_deny(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                FILE_DELETE_CHILD,
                setup,
            )?;
        }
        if !setup {
            check_file_access(token, path, file)?;
        }
    }
    for key in &keys {
        checked_deny(key.0, SE_REGISTRY_KEY, KEY_CREATE_LINK, setup)?;
        if !setup {
            check_key_access(token, key)?;
        }
    }
    Ok(())
}
/// Check the existing barriers on every payload startup, using the token of the suspended child.
/// The trusted caller has already compared the token's actual user, package and restricting SIDs.
pub fn verify_appcontainer_storage(
    token: HANDLE,
    user: &str,
    package: &str,
    expected_temp: &Path,
) -> Result<()> {
    check_storage(token, user, package, Some(expected_temp))
}
struct Attributes<'a>(Vec<usize>, std::marker::PhantomData<&'a ()>);
impl<'a> Attributes<'a> {
    fn new(count: u32) -> Result<Self> {
        let mut size = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), count, 0, &mut size);
        }
        if size == 0 || size > 65536 {
            return Err(fail("Invalid package setup attribute size"));
        }
        let mut values = vec![0usize; size.div_ceil(size_of::<usize>())];
        if unsafe {
            InitializeProcThreadAttributeList(values.as_mut_ptr().cast(), count, 0, &mut size)
        } == 0
        {
            return Err(last("Initialize setup attributes"));
        }
        Ok(Self(values, std::marker::PhantomData))
    }
    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }
    fn set<T>(&mut self, key: u32, value: &'a T) -> Result<()> {
        if unsafe {
            UpdateProcThreadAttribute(
                self.ptr(),
                0,
                key as usize,
                (value as *const T).cast(),
                size_of::<T>(),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(last("Set private package setup process attribute"));
        }
        Ok(())
    }
}
impl Drop for Attributes<'_> {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.ptr());
        }
    }
}
fn own_job() -> Result<OwnedHandle> {
    use windows_sys::Win32::System::JobObjects::*;
    let raw = unsafe { CreateJobObjectW(null(), null()) };
    if raw.is_null() {
        return Err(last("Create package setup cleanup job"));
    }
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(last("Set package setup cleanup barrier"));
    }
    Ok(job)
}
struct Suspended {
    process: OwnedHandle,
    thread: OwnedHandle,
}
impl Suspended {
    fn take(info: PROCESS_INFORMATION) -> Self {
        Self {
            process: unsafe { OwnedHandle::from_raw_handle(info.hProcess) },
            thread: unsafe { OwnedHandle::from_raw_handle(info.hThread) },
        }
    }
    fn stop(&self) -> Result<()> {
        let h = self.process.as_raw_handle();
        if unsafe { WaitForSingleObject(h, 0) } != WAIT_OBJECT_0
            && unsafe { TerminateProcess(h, 125) } == 0
        {
            return Err(last("Terminate trusted package setup probe"));
        }
        if unsafe { WaitForSingleObject(h, 15000) } != WAIT_OBJECT_0 {
            return Err(fail("Package setup process cleanup unconfirmed"));
        }
        Ok(())
    }
}
impl Drop for Suspended {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
fn seal_new_storage(user: &str, package: &str) -> Result<()> {
    let sid = parse_sid(package)?;
    let caps = SECURITY_CAPABILITIES {
        AppContainerSid: sid.0,
        Capabilities: null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let job = own_job()?;
    let job_value = job.as_raw_handle();
    let mut attributes = Attributes::new(2)?;
    attributes.set(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &caps)?;
    attributes.set(PROC_THREAD_ATTRIBUTE_JOB_LIST, &job_value)?;
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attributes.ptr();
    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    let system = system_directory()?;
    let image = wide(system.join("cmd.exe"))?;
    let local = super::super::windows::local_app_data()?;
    let windows = system
        .parent()
        .ok_or_else(|| fail("Missing Windows system root"))?;
    let mut environment = Vec::new();
    for (key, value) in [
        ("LOCALAPPDATA", local.as_path()),
        ("PATH", system.as_path()),
        ("SystemRoot", windows),
        ("WINDIR", windows),
    ] {
        environment.extend(wide(format!("{key}={}", value.display()))?);
    }
    environment.push(0);
    if unsafe {
        CreateProcessW(
            image.as_ptr(),
            null_mut(),
            null(),
            null(),
            0,
            CREATE_SUSPENDED
                | EXTENDED_STARTUPINFO_PRESENT
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            wide(&system)?.as_ptr(),
            &startup.StartupInfo,
            &mut info,
        )
    } == 0
    {
        return Err(last("Create suspended package identity for storage setup"));
    }
    let child = Suspended::take(info);
    let mut token = null_mut();
    if unsafe {
        OpenProcessToken(
            child.process.as_raw_handle(),
            TOKEN_QUERY | TOKEN_DUPLICATE,
            &mut token,
        )
    } == 0
    {
        return Err(last("Query setup package token"));
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let result = check_storage(token.as_raw_handle(), user, package, None);
    child.stop()?;
    result
}
struct NewPackage(Vec<u16>);
impl NewPackage {
    fn rollback(&self) -> Result<()> {
        hresult("Remove newly registered private profile", unsafe {
            DeleteAppContainerProfile(self.0.as_ptr())
        })
    }
}
/// The administrator launches this fixed entry point only as the newly created local account.
pub fn setup_account_package(user: &str, expected: &str, diagnostic: &Path) -> Result<()> {
    if current_sid()? != user || account_sid(expected)? != user {
        return Err(fail("Unexpected package setup caller"));
    }
    writer_name(expected)?;
    let mut log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(diagnostic)?;
    let result = setup_account_package_inner(user, expected);
    if let Err(error) = &result {
        let _ = writeln!(log, "{error}");
        let _ = log.sync_all();
    }
    result
}
fn setup_account_package_inner(user: &str, expected: &str) -> Result<()> {
    if current_sid()? != user || account_sid(expected)? != user {
        return Err(fail("Package setup requires the paired dedicated account"));
    }
    writer_name(expected)?;
    let mut h = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut h) } == 0 {
        return Err(last("Check setup identity"));
    }
    let token = unsafe { OwnedHandle::from_raw_handle(h) };
    for information in [TokenElevation, TokenIsAppContainer] {
        let (mut value, mut n) = (0u32, 0);
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                information,
                (&mut value as *mut u32).cast(),
                4,
                &mut n,
            )
        } == 0
            || value != 0
        {
            return Err(fail(
                "Package setup requires an ordinary dedicated-account token",
            ));
        }
    }
    if unsafe { IsTokenRestricted(token.as_raw_handle()) } != 0 {
        return Err(fail("Package setup refuses a restricted token"));
    }
    let name = moniker(user)?;
    match std::fs::symlink_metadata(local_packages()?.join(&name)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return Err(fail("Refuse an existing package storage location")),
        Err(e) => return Err(e.into()),
    }
    let name = wide(name)?;
    let mut sid = null_mut();
    hresult("Register new dedicated package profile", unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            name.as_ptr(),
            name.as_ptr(),
            null(),
            0,
            &mut sid,
        )
    })?;
    let profile = NewPackage(name);
    let registration = sid_text(sid);
    unsafe {
        FreeSid(sid);
    }
    let result = (|| {
        let actual = registration?;
        if actual != package_sid(user)? {
            return Err(fail(
                "Registered package SID differs from the account binding",
            ));
        }
        seal_new_storage(user, &actual)
    })();
    if let Err(e) = result {
        let cleanup = profile.rollback();
        return Err(fail(format!(
            "Package setup failed: {e}; profile rollback: {cleanup:?}"
        )));
    }
    Ok(())
}
pub(super) fn run_setup(manifest: &ProvisionManifest, password: &SecretPassword) -> Result<()> {
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    let log_path = manifest
        .workspace
        .join(format!("setup-{}.txt", uuid::Uuid::new_v4()));
    let job = own_job()?;
    let user = wide(&manifest.username)?;
    let domain = wide(".")?;
    let image = wide(&manifest.runner)?;
    let system = system_directory()?;
    let windows = system
        .parent()
        .ok_or_else(|| fail("Missing trusted Windows root"))?;
    let mut environment = Vec::new();
    for (k, p) in [
        ("PATH", &system as &Path),
        ("SystemRoot", windows),
        ("WINDIR", windows),
    ] {
        environment.extend(wide(format!("{k}={}", p.display()))?);
    }
    environment.push(0);
    let mut cmd = wide(format!(
        "\"{}\" --setup-package {} {} \"{}\"",
        manifest.runner.display(),
        manifest.user_sid,
        manifest.username,
        log_path.display()
    ))?;
    let mut startup: STARTUPINFOW = unsafe { zeroed() };
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    let mut info = unsafe { zeroed() };
    if unsafe {
        CreateProcessWithLogonW(
            user.as_ptr(),
            domain.as_ptr(),
            password.as_ptr(),
            LOGON_WITH_PROFILE,
            image.as_ptr(),
            cmd.as_mut_ptr(),
            CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            wide(&system)?.as_ptr(),
            &startup,
            &mut info,
        )
    } == 0
    {
        return Err(last("Start trusted private package setup"));
    }
    let child = Suspended::take(info);
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.process.as_raw_handle()) } == 0
    {
        return Err(last("Keep setup processes in the owned cleanup job"));
    }
    if unsafe { ResumeThread(child.thread.as_raw_handle()) } != 1 {
        return Err(last("Start dedicated-account profile setup"));
    }
    if unsafe { WaitForSingleObject(child.process.as_raw_handle(), 90000) } != WAIT_OBJECT_0 {
        return Err(fail("Dedicated package setup timed out"));
    }
    let mut code = 0;
    if unsafe { GetExitCodeProcess(child.process.as_raw_handle(), &mut code) } == 0 || code != 0 {
        let mut reason = String::new();
        if let Ok(file) = File::open(&log_path) {
            let _ = std::io::Read::take(file, 4096).read_to_string(&mut reason);
        }
        return Err(fail(format!(
            "Dedicated package setup refused (exit {code}): {reason}"
        )));
    }
    std::fs::remove_file(&log_path)?;
    Ok(())
}

fn file_identity(file: &File) -> Result<(u64, [u8; 16])> {
    let mut id: FILE_ID_INFO = unsafe { zeroed() };
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut id as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(last("Query pinned private profile identity"));
    }
    Ok((id.VolumeSerialNumber, id.FileId.Identifier))
}
fn open_profile_dir(path: &Path, access: u32) -> Result<File> {
    let file = OpenOptions::new()
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_dir() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(fail("Expected ordinary dedicated profile directory"));
    }
    Ok(file)
}
pub(super) struct FreshProfile {
    root: PathBuf,
    user: String,
    name: String,
    identity: (u64, [u8; 16]),
}
impl FreshProfile {
    pub(super) fn create(name: &str, user: &str) -> Result<Self> {
        let mut path = [0u16; MAX_PATH as usize];
        let max = path.len() as u32;
        hresult("Create new dedicated Windows user profile", unsafe {
            windows_sys::Win32::UI::Shell::CreateProfile(
                wide(user)?.as_ptr(),
                wide(name)?.as_ptr(),
                path.as_mut_ptr(),
                max,
            )
        })?;
        let n = path
            .iter()
            .position(|c| *c == 0)
            .ok_or_else(|| fail("Invalid new profile path"))?;
        let root = PathBuf::from(OsString::from_wide(&path[..n]));
        if root.file_name() != Some(OsStr::new(name)) || !root.is_absolute() {
            return Err(fail(format!(
                "Unexpected newly created profile location: {}",
                root.display()
            )));
        }
        let file = open_profile_dir(&root, FILE_READ_ATTRIBUTES | READ_CONTROL | FILE_TRAVERSE)?;
        Ok(Self {
            root,
            user: user.into(),
            name: name.into(),
            identity: file_identity(&file)?,
        })
    }
    pub(super) fn private_temp(&self, user: &str) -> Result<PathBuf> {
        if user != self.user {
            return Err(fail("Dedicated profile account changed"));
        }
        Ok(self
            .root
            .join("AppData")
            .join("Local")
            .join("Packages")
            .join(moniker(user)?)
            .join("AC")
            .join("Temp"))
    }
    pub(super) fn rollback(&self) -> Result<()> {
        if account_sid(&self.name)? != self.user
            || file_identity(&open_profile_dir(
                &self.root,
                FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
            )?)? != self.identity
        {
            return Err(fail("Cannot verify the new dedicated profile for rollback"));
        }
        if unsafe {
            windows_sys::Win32::UI::Shell::DeleteProfileW(
                wide(&self.user)?.as_ptr(),
                wide(&self.root)?.as_ptr(),
                null(),
            )
        } == 0
        {
            return Err(last("Remove only the newly created dedicated user profile"));
        }
        Ok(())
    }
    pub(super) fn bind_private_temp(&self, manifest: &ProvisionManifest, sddl: &str) -> Result<()> {
        if self.private_temp(&manifest.user_sid)? != manifest.private_temp {
            return Err(fail("Unexpected package temp layout"));
        }
        let mut paths: Vec<_> = manifest
            .private_temp
            .ancestors()
            .take_while(|p| p.starts_with(&self.root))
            .collect();
        paths.reverse();
        if paths.first() != Some(&self.root.as_path()) {
            return Err(fail("Package temp is outside the new user profile"));
        }
        let mut pinned = Vec::new();
        for path in paths {
            // MAXIMUM_ALLOWED prevents SetSecurityInfo from propagating an existing ancestor
            // ACL to children. Only these seven newly created directories may be touched.
            pinned.push(open_profile_dir(path, MAXIMUM_ALLOWED)?);
        }
        if file_identity(&pinned[0])? != self.identity {
            return Err(fail("New profile identity changed"));
        }
        let temp = pinned.pop().ok_or_else(|| fail("Missing package temp"))?;
        if std::fs::read_dir(&manifest.private_temp)?.next().is_some() {
            return Err(fail("Windows package temp is not fresh"));
        }
        for file in &pinned {
            grant_host_pin(file.as_raw_handle(), &manifest.host_sid)?;
        }
        let sd = descriptor(sddl)?;
        let (mut owner, mut dacl, mut label) = (null_mut(), null_mut(), null_mut());
        let (mut present, mut defaulted) = (0, 0);
        if unsafe { GetSecurityDescriptorOwner(sd.0, &mut owner, &mut defaulted) } == 0
            || unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) }
                == 0
            || present == 0
            || unsafe { GetSecurityDescriptorSacl(sd.0, &mut present, &mut label, &mut defaulted) }
                == 0
            || present == 0
        {
            return Err(fail("Invalid package temp security descriptor"));
        }
        status(
            "Bind the Windows package temp as the sole private write root",
            unsafe {
                SetSecurityInfo(
                    temp.as_raw_handle(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION
                        | DACL_SECURITY_INFORMATION
                        | PROTECTED_DACL_SECURITY_INFORMATION
                        | LABEL_SECURITY_INFORMATION,
                    owner,
                    null_mut(),
                    dacl,
                    label,
                )
            },
        )?;
        trusted_acl(
            temp.as_raw_handle(),
            &manifest.host_sid,
            &[
                (&manifest.user_sid, SANDBOX_MODIFY),
                (&manifest.appcontainer_sid, SANDBOX_MODIFY),
                (&manifest.writer_sid, SANDBOX_MODIFY),
            ],
        )?;
        verify_low_directory(temp.as_raw_handle())
    }
}
fn grant_host_pin(handle: HANDLE, host: &str) -> Result<()> {
    let sid = parse_sid(host)?;
    let (mut dacl, mut sd) = (null_mut(), null_mut());
    status("Read new dedicated profile ancestor ACL", unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    })?;
    let _sd = Local(sd);
    if dacl.is_null() {
        return Err(fail("Null DACL on new profile ancestor"));
    }
    let mut entry: EXPLICIT_ACCESS_W = unsafe { zeroed() };
    entry.grfAccessPermissions = FILE_READ_ATTRIBUTES | READ_CONTROL | FILE_TRAVERSE | SYNCHRONIZE;
    entry.grfAccessMode = GRANT_ACCESS;
    entry.grfInheritance = 0;
    entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
    entry.Trustee.TrusteeType = TRUSTEE_IS_USER;
    entry.Trustee.ptstrName = sid.0.cast();
    let mut updated = null_mut();
    status(
        "Build non-inherited individual host path-pin grant",
        unsafe { SetEntriesInAclW(1, &entry, dacl, &mut updated) },
    )?;
    let _updated = Local(updated.cast());
    status(
        "Apply metadata-only host path-pin grant to a new profile ancestor",
        unsafe {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                updated,
                null(),
            )
        },
    )
}
fn effective_access(
    token: HANDLE,
    handle: HANDLE,
    kind: SE_OBJECT_TYPE,
    mapping: &GENERIC_MAPPING,
) -> Result<u32> {
    let mut descriptor = null_mut();
    status("Read the pinned package security descriptor", unsafe {
        GetSecurityInfo(
            handle,
            kind,
            OWNER_SECURITY_INFORMATION
                | GROUP_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    })?;
    let descriptor = Local(descriptor);
    let mut duplicate = null_mut();
    if unsafe { DuplicateToken(token, SecurityImpersonation, &mut duplicate) } == 0 {
        return Err(last(
            "Duplicate the suspended token for package AccessCheck",
        ));
    }
    let duplicate = unsafe { OwnedHandle::from_raw_handle(duplicate) };
    let mut privileges = [0usize; 256];
    let mut bytes = std::mem::size_of_val(&privileges) as u32;
    let (mut granted, mut allowed) = (0, 0);
    if unsafe {
        AccessCheck(
            descriptor.0,
            duplicate.as_raw_handle(),
            MAXIMUM_ALLOWED,
            mapping,
            privileges.as_mut_ptr().cast(),
            &mut bytes,
            &mut granted,
            &mut allowed,
        )
    } == 0
    {
        return Err(last(
            "Check effective permissions on pinned package storage",
        ));
    }
    Ok(if allowed != 0 { granted } else { 0 })
}
fn verify_temp_access(token: HANDLE, path: &Path, pinned: &File) -> Result<()> {
    // A MAXIMUM_ALLOWED open on this writable object would request DELETE and
    // conflict with the host's deliberately non-delete-sharing directory pin.
    let mapping = GENERIC_MAPPING {
        GenericRead: FILE_GENERIC_READ,
        GenericWrite: FILE_GENERIC_WRITE,
        GenericExecute: FILE_GENERIC_EXECUTE,
        GenericAll: FILE_ALL_ACCESS,
    };
    let granted = effective_access(token, pinned.as_raw_handle(), SE_FILE_OBJECT, &mapping)?;
    let add = FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY;
    if granted & !SANDBOX_MODIFY != 0 || granted & add != add {
        return Err(fail(format!(
            "Unexpected effective write authority on package temp: {granted:#x}"
        )));
    }
    impersonate(token, || {
        let actual = open_profile_dir(path, add | FILE_READ_ATTRIBUTES)?;
        if file_identity(&actual)? != file_identity(pinned)? {
            return Err(fail(
                "Package temp changed during its effective access check",
            ));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn write_group_and_package_bind_to_the_dedicated_account() {
        let a = "S-1-5-21-111-222-333-1001";
        let b = "S-1-5-21-111-222-333-1002";
        let first = package_sid(a).unwrap();
        assert!(first.starts_with("S-1-15-2-"));
        assert_eq!(package_sid(a).unwrap(), first);
        assert_ne!(package_sid(b).unwrap(), first);
        assert!(package_sid("S-1-5-21-not-a-sid").is_err());
        assert_eq!(
            writer_name("wwbox_0123456789ab").unwrap(),
            "wwrite_0123456789ab"
        );
        assert!(writer_name("ordinary-user").is_err());
    }
    #[test]
    fn writable_directory_requires_an_inheritable_low_mandatory_label() {
        let temp = tempfile::tempdir().unwrap();
        let host = current_sid().unwrap();
        for (name, label, expected) in [
            ("low", "S:(ML;OICI;NW;;;LW)", true),
            ("not-inherited", "S:(ML;;NW;;;LW)", false),
            ("ordinary", "", false),
        ] {
            let path = temp.path().join(name);
            directory(&path, &format!("O:{host}D:P(A;;FA;;;{host}){label}")).unwrap();
            let f = OpenOptions::new()
                .access_mode(READ_CONTROL)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(&path)
                .unwrap();
            assert_eq!(
                verify_low_directory(f.as_raw_handle()).is_ok(),
                expected,
                "{name}"
            );
        }
    }
}
