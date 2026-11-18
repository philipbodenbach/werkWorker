//! Temporary access to the registered package's own kernel-object directory.
//! The host retains the original descriptor and restores it after Job drainage.
use super::windows_tree_runner;
use crate::core::{Result, WorkerError};
use std::{
    ffi::c_void,
    mem::{size_of, zeroed},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, Isolation::GetAppContainerNamedObjectPath, *},
    Storage::FileSystem::{READ_CONTROL, WRITE_DAC},
    System::LibraryLoader::{GetModuleHandleW, GetProcAddress},
};

const DIRECTORY_CREATE: u32 = 0x000f;
const DIRECTORY_ACCESS: u32 = READ_CONTROL | WRITE_DAC;
const GROUP_LOGON_ID: u32 = 0xc0000000;

fn invalid(message: impl Into<String>) -> WorkerError {
    WorkerError::new("sandbox_unavailable", message)
}
fn last(operation: &str) -> WorkerError {
    invalid(format!("{operation}: {}", std::io::Error::last_os_error()))
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
        return Err(invalid("Invalid package namespace SID"));
    }
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(last("Format package namespace SID"));
    }
    let _value = Local(text.cast());
    let mut size = 0;
    unsafe {
        while *text.add(size) != 0 {
            size += 1;
        }
        String::from_utf16(std::slice::from_raw_parts(text, size))
            .map_err(|_| invalid("Invalid formatted SID"))
    }
}
fn directory_path(token: HANDLE, package: &str) -> Result<(u32, String)> {
    let mut session = 0u32;
    let mut bytes = 0;
    if unsafe {
        GetTokenInformation(
            token,
            TokenSessionId,
            (&mut session as *mut u32).cast(),
            4,
            &mut bytes,
        )
    } == 0
        || bytes != 4
        || session == 0
    {
        return Err(last("Read AppContainer session"));
    }
    let mut out = 0;
    let mut buffer = [0u16; 512];
    if unsafe {
        GetAppContainerNamedObjectPath(
            token,
            null_mut(),
            buffer.len() as u32,
            buffer.as_mut_ptr(),
            &mut out,
        )
    } == 0
    {
        return Err(last("Read AppContainer object directory"));
    }
    let end = buffer
        .iter()
        .position(|ch| *ch == 0)
        .ok_or_else(|| invalid("Invalid package object directory length"))?;
    let actual = String::from_utf16(&buffer[..end])
        .map_err(|_| invalid("Invalid package object directory"))?;
    let expected = format!(r"AppContainerNamedObjects\{package}");
    if actual != expected {
        return Err(invalid(
            "AppContainer object directory differs from provisioned identity",
        ));
    }
    Ok((session, format!(r"\Sessions\{session}\{expected}")))
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: HANDLE,
    object_name: *mut UNICODE_STRING,
    attributes: u32,
    security_descriptor: *mut c_void,
    security_quality_of_service: *mut c_void,
}
type NtOpenDirectory = unsafe extern "system" fn(
    handle: *mut HANDLE,
    access: u32,
    attributes: *const ObjectAttributes,
) -> i32;
fn open_directory(path: &str, access: u32) -> Result<OwnedHandle> {
    let mut name: Vec<_> = path.encode_utf16().collect();
    let bytes = u16::try_from(name.len() * size_of::<u16>())
        .map_err(|_| invalid("Oversized namespace path"))?;
    let mut name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: name.as_mut_ptr(),
    };
    let attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: null_mut(),
        object_name: &mut name,
        attributes: 0x40, // OBJ_CASE_INSENSITIVE; no handle inheritance.
        security_descriptor: null_mut(),
        security_quality_of_service: null_mut(),
    };
    let mut handle = null_mut();
    // Resolve this native API as documented rather than requiring its import
    // library or a static import on every unrelated single-process invocation.
    // https://learn.microsoft.com/windows/win32/devnotes/ntopendirectoryobject
    let module_name: Vec<_> = "ntdll.dll".encode_utf16().chain([0]).collect();
    let module = unsafe { GetModuleHandleW(module_name.as_ptr()) };
    if module.is_null() {
        return Err(last("Locate Windows native namespace APIs"));
    }
    let entry = unsafe { GetProcAddress(module, c"NtOpenDirectoryObject".as_ptr().cast()) }
        .ok_or_else(|| invalid("Windows native namespace API is unavailable"))?;
    let open: NtOpenDirectory = unsafe { std::mem::transmute(entry) };
    let result = unsafe { open(&mut handle, access, &attributes) };
    if result < 0 || handle.is_null() {
        return Err(invalid(format!(
            "Open private package object directory: NTSTATUS {result:#x}"
        )));
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

pub(super) fn open_for_payload(token: HANDLE, package: &str) -> Result<OwnedHandle> {
    let (_, path) = directory_path(token, package)?;
    open_directory(&path, DIRECTORY_ACCESS)
}

// Both documented query results begin with UNICODE_STRING. Pin the string in
// the returned buffer and validate its range before using it for comparison.
pub(crate) type NtQueryObject =
    unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;
pub(crate) fn nt_query_object() -> Result<NtQueryObject> {
    let library: Vec<_> = "ntdll.dll".encode_utf16().chain([0]).collect();
    let module = unsafe { GetModuleHandleW(library.as_ptr()) };
    if module.is_null() {
        return Err(last("Locate native object inspection API"));
    }
    let proc = unsafe { GetProcAddress(module, c"NtQueryObject".as_ptr().cast()) }
        .ok_or_else(|| invalid("Native object inspection unavailable"))?;
    Ok(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, NtQueryObject>(proc) })
}

pub(crate) fn object_name(handle: HANDLE) -> Result<String> {
    object_information(handle, 1)
}

fn object_information(handle: HANDLE, class: u32) -> Result<String> {
    let query = nt_query_object()?;
    let mut bytes = 0;
    unsafe { query(handle, class, null_mut(), 0, &mut bytes) };
    if bytes < size_of::<UNICODE_STRING>() as u32 || bytes > 65536 {
        return Err(invalid("Invalid native object information length"));
    }
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    let status = unsafe { query(handle, class, buffer.as_mut_ptr().cast(), bytes, &mut bytes) };
    if status < 0 {
        return Err(invalid(format!(
            "Query package native object identity: {status:#x}"
        )));
    }
    let info = unsafe { &*buffer.as_ptr().cast::<UNICODE_STRING>() };
    let base = buffer.as_ptr() as usize;
    let begin = info.Buffer as usize;
    let len = info.Length as usize;
    if len == 0
        || !len.is_multiple_of(2)
        || !begin.is_multiple_of(2)
        || begin < base
        || begin.checked_add(len).is_none_or(|end| {
            end > base + bytes as usize || end > base + std::mem::size_of_val(buffer.as_slice())
        })
    {
        return Err(invalid("Invalid native object identity range"));
    }
    String::from_utf16(unsafe { std::slice::from_raw_parts(info.Buffer, len / 2) })
        .map_err(|_| invalid("Invalid native object name"))
}
fn logon_sids(token: HANDLE) -> Result<Vec<String>> {
    let buffer = windows_tree_runner::token_information(token, TokenGroups)
        .map_err(|err| invalid(err.to_string()))?;
    windows_tree_runner::token_groups(&buffer)
        .map_err(|err| invalid(err.to_string()))?
        .iter()
        .filter(|entry| entry.Attributes & GROUP_LOGON_ID == GROUP_LOGON_ID)
        .map(|entry| sid_text(entry.Sid))
        .collect()
}
struct HostIdentity {
    user: String,
    logon: String,
}
impl HostIdentity {
    fn current(expected_user: &str, session: u32) -> Result<Self> {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        let mut token = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(last("Open the namespace supervisor's primary token"));
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let info = |class| {
            windows_tree_runner::token_information(token.as_raw_handle(), class)
                .map_err(|error| invalid(error.to_string()))
        };
        let user = info(TokenUser)?;
        let user = sid_text(unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid })?;
        let actual_session = info(TokenSessionId)?;
        let elevated = info(TokenElevation)?;
        let appcontainer = info(TokenIsAppContainer)?;
        if user != expected_user
            || unsafe { *actual_session.as_ptr().cast::<u32>() } != session
            || unsafe { *elevated.as_ptr().cast::<u32>() } != 0
            || unsafe { *appcontainer.as_ptr().cast::<u32>() } != 0
        {
            return Err(invalid(
                "Unexpected host identity for the private package namespace",
            ));
        }
        let groups = info(TokenLogonSid)?;
        let [entry] = windows_tree_runner::token_groups(&groups)
            .map_err(|error| invalid(error.to_string()))?
        else {
            return Err(invalid("Expected exactly one host logon SID"));
        };
        const ENABLED_LOGON: u32 = GROUP_LOGON_ID | 4;
        const DENY_ONLY: u32 = 0x10;
        if entry.Attributes & (ENABLED_LOGON | DENY_ONLY) != ENABLED_LOGON {
            return Err(invalid("Host logon SID must be enabled"));
        }
        Ok(Self {
            user,
            logon: sid_text(entry.Sid)?,
        })
    }
    fn allows_existing_ace(&self, sid: &str, mask: u32, flags: u8) -> bool {
        // Windows can grant the interactive host direct directory access
        // and an inherit-only template for its individual logon.
        let inherit_only = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE | INHERIT_ONLY_ACE) as u8;
        (sid == self.user || sid == self.logon) && flags == 0 && mask & !0x000f000f == 0
            || sid == self.logon && flags == inherit_only && mask == GENERIC_ALL
    }
}
struct SavedDescriptor(Vec<usize>);
impl SavedDescriptor {
    fn read(handle: HANDLE) -> Result<Self> {
        let mut needed = 0;
        unsafe {
            GetKernelObjectSecurity(
                handle,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                null_mut(),
                0,
                &mut needed,
            );
        }
        if !(20..=1024 * 1024).contains(&needed) {
            return Err(last("Size private namespace descriptor"));
        }
        let mut data = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        if unsafe {
            GetKernelObjectSecurity(
                handle,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                data.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(last("Read private namespace descriptor"));
        }
        Ok(Self(data))
    }
    fn ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.0.as_ptr().cast_mut().cast()
    }
    fn owner(&self) -> Result<String> {
        let (mut owner, mut defaulted) = (null_mut(), 0);
        if unsafe { GetSecurityDescriptorOwner(self.ptr(), &mut owner, &mut defaulted) } == 0 {
            return Err(last("Read package directory owner"));
        }
        sid_text(owner)
    }
    fn acl(&self) -> Result<*mut ACL> {
        let (mut acl, mut present, mut defaulted) = (null_mut(), 0, 0);
        if unsafe { GetSecurityDescriptorDacl(self.ptr(), &mut present, &mut acl, &mut defaulted) }
            == 0
            || present == 0
            || acl.is_null()
            || unsafe { IsValidAcl(acl) } == 0
        {
            return Err(invalid("Private namespace has an invalid DACL"));
        }
        Ok(acl)
    }
    fn protected(&self) -> Result<u16> {
        let (mut flags, mut rev) = (0, 0);
        if unsafe { GetSecurityDescriptorControl(self.ptr(), &mut flags, &mut rev) } == 0 {
            return Err(last("Read package DACL protection"));
        }
        Ok(flags & SE_DACL_PROTECTED)
    }
    fn matches(&self, owner: &str, protected: u16, acl: *const ACL) -> Result<bool> {
        let current = self.acl()?;
        Ok(self.owner()? == owner
            && self.protected()? == protected
            && unsafe {
                (*current).AclSize == (*acl).AclSize
                    && std::slice::from_raw_parts(current.cast::<u8>(), (*current).AclSize as usize)
                        == std::slice::from_raw_parts(acl.cast::<u8>(), (*acl).AclSize as usize)
            })
    }
}
// These are the Windows-created principals on this specific package directory.
// A broad pre-existing grant to any other principal would invalidate the lease.
fn check_principals(
    acl: *mut ACL,
    session: u32,
    account: &str,
    package: &str,
    writer: &str,
    logons: &[String],
    host: &HostIdentity,
) -> Result<()> {
    let window_manager = format!("S-1-5-90-0-{session}");
    let mut package_present = false;
    for i in 0..unsafe { (*acl).AceCount } as u32 {
        let mut p = null_mut();
        if unsafe { GetAce(acl, i, &mut p) } == 0 {
            return Err(last("Read private namespace ACE"));
        }
        let header = unsafe { &*p.cast::<ACE_HEADER>() };
        if header.AceType != 0 && header.AceType != 1
            || (header.AceSize as usize) < size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Err(invalid("Unexpected ACE type in private package directory"));
        }
        let ace = unsafe { &*p.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = sid_text(std::ptr::addr_of!(ace.SidStart).cast_mut().cast())?;
        if sid == writer {
            return Err(invalid(
                "The package directory already contains the temporary writer grant",
            ));
        }
        if header.AceType == 1 {
            continue;
        }
        if sid == package {
            package_present = true;
        }
        if [
            account,
            package,
            window_manager.as_str(),
            "S-1-5-18",
            "S-1-5-32-544",
        ]
        .contains(&sid.as_str())
            || logons.contains(&sid)
            || host.allows_existing_ace(&sid, ace.Mask, header.AceFlags)
        {
            continue;
        }
        if sid == "S-1-3-0" && header.AceFlags & INHERIT_ONLY_ACE as u8 != 0 {
            continue;
        }
        if ["S-1-1-0", "S-1-5-12"].contains(&sid.as_str()) && ace.Mask & !0x3 == 0 {
            continue;
        }
        return Err(invalid(
            "Private package directory has an unexpected access grant",
        ));
    }
    if !package_present {
        return Err(invalid(
            "Individual package grant missing from private directory",
        ));
    }
    Ok(())
}
fn verify_directory_handle(handle: HANDLE, expected_path: &str) -> Result<()> {
    let mut flags = 0;
    if unsafe { GetHandleInformation(handle, &mut flags) } == 0 || flags & HANDLE_FLAG_INHERIT != 0
    {
        return Err(invalid(
            "Package directory handle is invalid or inheritable",
        ));
    }
    if object_information(handle, 2)? != "Directory" {
        return Err(invalid("Handle does not identify a native Directory"));
    }
    if object_name(handle)? != expected_path {
        return Err(invalid(
            "Native package directory name differs from the token-bound path",
        ));
    }
    Ok(())
}

pub(super) struct PackageNamespaceGuard {
    handle: OwnedHandle,
    old: SavedDescriptor,
    owner: String,
    protected: u16,
    updated: Vec<usize>,
    attempted: bool,
}
impl PackageNamespaceGuard {
    pub(super) fn prepare(
        handle: OwnedHandle,
        session: u32,
        host: &str,
        account: &str,
        package: &str,
        writer: &str,
        payload_token: HANDLE,
    ) -> Result<Self> {
        windows_tree_runner::verify_appcontainer_token(payload_token, account, package, writer)
            .map_err(|e| invalid(e.to_string()))?;
        let (actual_session, path) = directory_path(payload_token, package)?;
        let h = handle.as_raw_handle();
        if session != actual_session {
            return Err(invalid(
                "Unexpected AppContainer session for the named object directory",
            ));
        }
        verify_directory_handle(h, &path)?;
        let old = SavedDescriptor::read(h)?;
        let owner = old.owner()?;
        if ![account, "S-1-5-18", "S-1-5-32-544"].contains(&owner.as_str()) {
            return Err(invalid("Untrusted private package directory owner"));
        }
        let protected = old.protected()?;
        let acl = old.acl()?;
        check_principals(
            acl,
            session,
            account,
            package,
            writer,
            &logon_sids(payload_token)?,
            &HostIdentity::current(host, session)?,
        )?;
        let writer_text: Vec<_> = writer.encode_utf16().chain([0]).collect();
        let mut sid = null_mut();
        if unsafe { ConvertStringSidToSidW(writer_text.as_ptr(), &mut sid) } == 0 {
            return Err(last("Read package write restriction"));
        }
        let _sid = Local(sid);
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: DIRECTORY_CREATE,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: sid.cast(),
            },
        };
        let mut changed = null_mut();
        let status = unsafe { SetEntriesInAclW(1, &entry, acl, &mut changed) };
        if status != ERROR_SUCCESS || changed.is_null() {
            return Err(invalid(format!(
                "Prepare private package directory grant: {status}"
            )));
        }
        let _allocation = Local(changed.cast());
        if unsafe { IsValidAcl(changed) } == 0 {
            return Err(invalid("Windows produced an invalid package directory ACL"));
        }
        let len = usize::from(unsafe { (*changed).AclSize });
        let mut updated = vec![0usize; len.div_ceil(size_of::<usize>())];
        unsafe {
            std::ptr::copy_nonoverlapping(changed.cast::<u8>(), updated.as_mut_ptr().cast(), len);
        }
        Ok(Self {
            handle,
            old,
            owner,
            protected,
            updated,
            attempted: false,
        })
    }
    fn matches(&self, acl: *const ACL) -> Result<bool> {
        SavedDescriptor::read(self.handle.as_raw_handle())?.matches(
            &self.owner,
            self.protected,
            acl,
        )
    }
    pub(super) fn apply(&mut self) -> Result<()> {
        if self.attempted || !self.matches(self.old.acl()?)? {
            return Err(invalid(
                "Private package directory changed before installation",
            ));
        }
        let mut sd: SECURITY_DESCRIPTOR = unsafe { zeroed() };
        let p = (&mut sd as *mut SECURITY_DESCRIPTOR).cast();
        if unsafe { InitializeSecurityDescriptor(p, 1) } == 0
            || unsafe {
                SetSecurityDescriptorDacl(p, 1, self.updated.as_ptr().cast_mut().cast(), 0)
            } == 0
            || unsafe { SetSecurityDescriptorControl(p, SE_DACL_PROTECTED, self.protected) } == 0
        {
            return Err(last("Prepare private directory descriptor"));
        }
        self.attempted = true;
        if unsafe {
            SetKernelObjectSecurity(self.handle.as_raw_handle(), DACL_SECURITY_INFORMATION, p)
        } == 0
        {
            return Err(last("Apply private package directory grant"));
        }
        if !self.matches(self.updated.as_ptr().cast_mut().cast())? {
            return Err(invalid("Private package directory grant did not persist"));
        }
        Ok(())
    }
    pub(super) fn restore(&mut self) -> Result<()> {
        if !self.attempted {
            return Ok(());
        }
        let old = self.old.acl()?;
        if !self.matches(old)? {
            if !self.matches(self.updated.as_ptr().cast_mut().cast())? {
                return Err(invalid(
                    "Private package directory changed during execution; restore requires review",
                ));
            }
            if unsafe {
                SetKernelObjectSecurity(
                    self.handle.as_raw_handle(),
                    DACL_SECURITY_INFORMATION,
                    self.old.ptr(),
                )
            } == 0
            {
                return Err(last("Restore private package directory descriptor"));
            }
            if !self.matches(old)? {
                return Err(invalid(
                    "Private package directory restoration could not be confirmed",
                ));
            }
        }
        self.attempted = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Threading::{
        CreateEventW, GetCurrentProcess, OpenProcessToken,
    };

    const ACCOUNT: &str = "S-1-5-21-1-2-3-1001";
    const PACKAGE: &str = "S-1-15-2-5-6-7-8-9-10-11";
    const WRITER: &str = "S-1-5-21-1-2-3-1003";
    const HOST: &str = "S-1-5-21-1-2-3-1004";
    const HOST_LOGON: &str = "S-1-5-5-10-11";

    fn validate_synthetic_grants(
        extra: &str,
        session: u32,
        account: &str,
        package: &str,
    ) -> Result<()> {
        let text = format!(
            "O:SYD:P(A;;0x3;;;WD)(A;;0x2;;;RC)(A;;0xf000f;;;S-1-5-90-0-1)(A;;0xf000f;;;SY)(A;;0xf000f;;;{ACCOUNT})(A;;0xf000f;;;{PACKAGE}){extra}"
        );
        let wide: Vec<_> = text.encode_utf16().chain([0]).collect();
        let mut sd = null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    1,
                    &mut sd,
                    null_mut(),
                )
            },
            0
        );
        let _memory = Local(sd);
        let (mut present, mut acl, mut defaulted) = (0, null_mut(), 0);
        assert_ne!(
            unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted) },
            0
        );
        assert_ne!(present, 0);
        check_principals(
            acl,
            session,
            account,
            package,
            WRITER,
            &[],
            &HostIdentity {
                user: HOST.into(),
                logon: HOST_LOGON.into(),
            },
        )
    }

    #[test]
    fn rejects_unrelated_namespace_mutation_even_beside_exact_package_grants() {
        validate_synthetic_grants("", 1, ACCOUNT, PACKAGE).unwrap();
        let host_template =
            format!("(A;;0xf000f;;;{HOST})(A;;0xf000f;;;{HOST_LOGON})(A;OICIIO;GA;;;{HOST_LOGON})");
        validate_synthetic_grants(&host_template, 1, ACCOUNT, PACKAGE).unwrap();
        for extra in [
            "(A;;0xf000f;;;S-1-5-21-1-2-3-1005)",
            "(A;OICIIO;GA;;;S-1-5-5-10-12)",
            "(A;;0xf000f;;;S-1-5-5-10-12)",
            "(A;OICI;GA;;;S-1-5-5-10-11)",
            "(A;OICIIO;GA;;;S-1-5-21-1-2-3-1004)",
            "(A;;0x40000;;;WD)",
            "(A;;0x80000;;;BU)",
            "(A;;0x10000;;;AU)",
            "(A;;GA;;;S-1-5-21-1-2-3-1002)",
        ] {
            assert!(
                validate_synthetic_grants(extra, 1, ACCOUNT, PACKAGE).is_err(),
                "{extra}"
            );
        }
    }

    #[test]
    fn distinct_provisions_cannot_reuse_another_packages_namespace() {
        assert!(validate_synthetic_grants("", 2, ACCOUNT, PACKAGE).is_err());
        assert!(validate_synthetic_grants("", 1, "S-1-5-21-1-2-3-1002", PACKAGE).is_err());
        assert!(validate_synthetic_grants("", 1, ACCOUNT, "S-1-15-2-5-6-7-8-9-10-12").is_err());
        assert!(
            validate_synthetic_grants(&format!("(A;;0xf;;;{WRITER})"), 1, ACCOUNT, PACKAGE)
                .is_err()
        );
    }

    #[test]
    fn native_package_namespace_checks_type_and_exact_name_before_acl_access() {
        // Both handles are query-only inputs to the same pre-mutation validator.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        assert!(!event.is_null());
        let event = unsafe { OwnedHandle::from_raw_handle(event) };
        let wrong_type = verify_directory_handle(event.as_raw_handle(), "\\").unwrap_err();
        assert!(
            wrong_type.to_string().contains("native Directory"),
            "{wrong_type}"
        );
        let mut token = null_mut();
        assert_ne!(
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
            0
        );
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let buf =
            windows_tree_runner::token_information(token.as_raw_handle(), TokenSessionId).unwrap();
        let session = unsafe { *buf.as_ptr().cast::<u32>() };
        let current = if session == 0 {
            "\\BaseNamedObjects".to_owned()
        } else {
            format!("\\Sessions\\{session}\\BaseNamedObjects")
        };
        let root = open_directory(&current, 1).unwrap();
        verify_directory_handle(root.as_raw_handle(), &current).unwrap();
        let wrong_path = verify_directory_handle(
            root.as_raw_handle(),
            "\\Sessions\\1\\AppContainerNamedObjects\\unrelated",
        )
        .unwrap_err();
        assert!(
            wrong_path.to_string().contains("token-bound path"),
            "{wrong_path}"
        );
    }
}
