//! Explicit account grants for Win32 named synchronization objects.
//!
//! A restricted token's Base named-object directory is the current session's
//! BaseNamedObjects\Restricted directory. KernelBase opens that directory with
//! DIRECTORY_QUERY | DIRECTORY_TRAVERSE | DIRECTORY_CREATE_OBJECT |
//! DIRECTORY_CREATE_SUBDIRECTORY even when creating just a semaphore. A
//! create-object-only ACE therefore does not make Rustc's jobserver work.
//! Only administrator provisioning changes its DACL; ordinary launch is read-only.
use crate::core::{Result, WorkerError};
use serde::{Deserialize, Serialize};
use std::{
    ffi::c_void,
    marker::PhantomData,
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::{addr_of, null_mut},
    rc::Rc,
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::{READ_CONTROL, WRITE_DAC},
    System::LibraryLoader::{GetModuleHandleW, GetProcAddress},
    System::Threading::{
        CreateMutexExW, GetCurrentProcessId, MUTEX_ALL_ACCESS, ReleaseMutex, WaitForSingleObject,
    },
};

const ACCOUNT_ACCESS: u32 = 0x000f;
const HOST_ACCESS: u32 = READ_CONTROL | 0x0003;
const SYSTEM: &str = "S-1-5-18";
const ADMIN: &str = "S-1-5-32-544";
const ALLOW_ACE: u8 = 0;
const DENY_ACE: u8 = 1;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: HANDLE,
    object_name: *mut UnicodeString,
    attributes: u32,
    security_descriptor: *mut c_void,
    security_quality_of_service: *mut c_void,
}
type NtOpenDirectory = unsafe extern "system" fn(
    handle: *mut HANDLE,
    access: u32,
    attributes: *const ObjectAttributes,
) -> i32;
#[link(name = "kernel32")]
unsafe extern "system" {
    fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
}

/// This session-scoped prerequisite must be re-provisioned after session change
/// or reboot. It is not a claim that the full process-tree profile is qualified.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub session_id: u32,
    pub path: String,
    pub owner_sid: String,
}

fn fail(message: impl Into<String>) -> WorkerError {
    WorkerError::new("sandbox_unavailable", message)
}
fn last(operation: &str) -> WorkerError {
    fail(format!("{operation}: {}", std::io::Error::last_os_error()))
}
fn status(operation: &str, result: u32) -> Result<()> {
    if result == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(fail(format!("{operation}: Windows error {result:#x}")))
    }
}
struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}
fn parse_sid(text: &str) -> Result<Local> {
    let text: Vec<_> = text.encode_utf16().chain([0]).collect();
    let mut sid = null_mut();
    if unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut sid) } == 0 {
        return Err(last("Parse namespace principal SID"));
    }
    Ok(Local(sid))
}
fn sid_text(sid: PSID) -> Result<String> {
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(fail("Invalid namespace principal SID"));
    }
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(last("Format namespace principal SID"));
    }
    let _memory = Local(text.cast());
    let mut length = 0;
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            text, length,
        )))
    }
}
fn current_namespace() -> Result<(u32, String)> {
    let mut session = 0;
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
        return Err(last("Read named-object namespace session"));
    }
    if session == 0 {
        return Err(fail(
            "The Windows process-tree profile requires an interactive host session; session zero is unsupported",
        ));
    }
    Ok((
        session,
        format!(r"\Sessions\{session}\BaseNamedObjects\Restricted"),
    ))
}
fn open(path: &str, access: u32) -> Result<OwnedHandle> {
    let mut name: Vec<_> = path.encode_utf16().collect();
    let bytes = u16::try_from(name.len() * size_of::<u16>())
        .map_err(|_| fail("Oversized namespace path"))?;
    let mut name = UnicodeString {
        length: bytes,
        maximum_length: bytes,
        buffer: name.as_mut_ptr(),
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
        .ok_or_else(|| fail("Windows native namespace API is unavailable"))?;
    let open: NtOpenDirectory = unsafe { std::mem::transmute(entry) };
    let result = unsafe { open(&mut handle, access, &attributes) };
    if result < 0 || handle.is_null() {
        return Err(fail(format!(
            "Open restricted named-object namespace: NTSTATUS {result:#x}; explicit administrator provisioning for the current session is required"
        )));
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

#[derive(Clone, PartialEq, Eq)]
struct Descriptor {
    owner: String,
    acl: Vec<u8>,
    protected: bool,
}
impl Descriptor {
    fn read(handle: HANDLE) -> Result<Self> {
        let mut owner = null_mut();
        let mut acl = null_mut();
        let mut descriptor = null_mut();
        status("Read named-object namespace security", unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut descriptor,
            )
        })?;
        let _descriptor = Local(descriptor);
        if acl.is_null() || unsafe { IsValidAcl(acl) } == 0 {
            return Err(fail("Named-object namespace has an invalid or null DACL"));
        }
        let owner = sid_text(owner)?;
        if ![SYSTEM, ADMIN].contains(&owner.as_str()) {
            return Err(fail(
                "Named-object namespace owner is not SYSTEM or Administrators",
            ));
        }
        let mut control = 0;
        let mut revision = 0;
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
            return Err(last("Read named-object namespace DACL protection"));
        }
        Ok(Self {
            owner,
            acl: unsafe {
                std::slice::from_raw_parts(acl.cast::<u8>(), (*acl).AclSize as usize).to_vec()
            },
            protected: control & SE_DACL_PROTECTED != 0,
        })
    }
    fn pointer(&self) -> *mut ACL {
        self.acl.as_ptr().cast_mut().cast()
    }
    fn write(&self, handle: HANDLE) -> Result<()> {
        status("Apply named-object namespace DACL", unsafe {
            SetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION
                    | if self.protected {
                        PROTECTED_DACL_SECURITY_INFORMATION
                    } else {
                        UNPROTECTED_DACL_SECURITY_INFORMATION
                    },
                null_mut(),
                null_mut(),
                self.pointer(),
                null_mut(),
            )
        })
    }
    fn exact_grant(&self, principal: &str, expected: u32) -> Result<bool> {
        let mut found = false;
        for index in 0..unsafe { (*self.pointer()).AceCount } as u32 {
            let mut ace = null_mut();
            if unsafe { GetAce(self.pointer(), index, &mut ace) } == 0 {
                return Err(last("Read named-object namespace ACE"));
            }
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            if ![ALLOW_ACE, DENY_ACE].contains(&header.AceType) {
                return Err(fail("Unsupported named-object namespace ACE type"));
            }
            let allow = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            if sid_text(addr_of!(allow.SidStart).cast_mut().cast())? == principal {
                if found
                    || header.AceType != ALLOW_ACE
                    || header.AceFlags != 0
                    || allow.Mask != expected
                {
                    return Err(fail(
                        "Named-object namespace principal has an unexpected grant",
                    ));
                }
                found = true;
            }
        }
        Ok(found)
    }
    fn verify_safety(&self, session: u32) -> Result<()> {
        let window_manager = format!("S-1-5-90-0-{session}");
        for index in 0..unsafe { (*self.pointer()).AceCount } as u32 {
            let mut ace = null_mut();
            if unsafe { GetAce(self.pointer(), index, &mut ace) } == 0 {
                return Err(last("Read namespace boundary ACE"));
            }
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            if ![ALLOW_ACE, DENY_ACE].contains(&header.AceType) {
                return Err(fail("Unsupported named-object namespace ACE type"));
            }
            let allow = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            let principal = sid_text(addr_of!(allow.SidStart).cast_mut().cast())?;
            if header.AceType == ALLOW_ACE
                && ![SYSTEM, ADMIN, window_manager.as_str()].contains(&principal.as_str())
                && allow.Mask & !(ACCOUNT_ACCESS | HOST_ACCESS) != 0
            {
                return Err(fail(
                    "Named-object namespace grants mutation authority to an untrusted principal",
                ));
            }
        }
        Ok(())
    }
    fn verify(&self, host: &str, account: &str) -> Result<()> {
        if !self.exact_grant(account, ACCOUNT_ACCESS)? || !self.exact_grant(host, HOST_ACCESS)? {
            return Err(fail(
                "Required named-object namespace grants are absent; explicitly re-provision for the current host session",
            ));
        }
        Ok(())
    }
}

/// Keep the inspected Directory object alive through the invocation. No write
/// access or mutation occurs in this ordinary-host path.
pub(super) struct Pin {
    _handle: OwnedHandle,
}
pub(super) fn verify(receipt: &Receipt, host: &str, account: &str) -> Result<Pin> {
    let (session, path) = current_namespace()?;
    if receipt.session_id != session || receipt.path != path {
        return Err(fail(
            "Named-object namespace session changed; explicit administrator re-provisioning is required",
        ));
    }
    let handle = open(&path, HOST_ACCESS)?;
    let descriptor = Descriptor::read(handle.as_raw_handle())?;
    if descriptor.owner != receipt.owner_sid {
        return Err(fail("Named-object namespace owner changed"));
    }
    descriptor.verify_safety(session)?;
    descriptor.verify(host, account)?;
    Ok(Pin { _handle: handle })
}

// The named Directory is shared by independent provisions. Serialize this
// implementation's read/modify/write transaction across administrator processes
// so two successful setups cannot discard each other's grants. This mutex is
// never opened or created by ordinary runtime loading.
struct SetupLock {
    handle: OwnedHandle,
    // Win32 mutex ownership belongs to the acquiring thread.
    _thread_bound: PhantomData<Rc<()>>,
}
impl SetupLock {
    fn acquire(session: u32) -> Result<Self> {
        let sddl: Vec<_> = "O:BAD:P(A;;GA;;;SY)(A;;GA;;;BA)"
            .encode_utf16()
            .chain([0])
            .collect();
        let mut sd = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut sd,
                null_mut(),
            )
        } == 0
        {
            return Err(last("Build administrator namespace setup lock ACL"));
        }
        let _sd = Local(sd);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let name: Vec<_> = format!(r"Global\WerkWorkerNamespaceSetup-{session}")
            .encode_utf16()
            .chain([0])
            .collect();
        let handle = unsafe { CreateMutexExW(&attributes, name.as_ptr(), 0, MUTEX_ALL_ACCESS) };
        if handle.is_null() {
            return Err(last("Open administrator namespace setup lock"));
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let actual = Descriptor::read(handle.as_raw_handle())?;
        if actual.owner != ADMIN || !actual.protected {
            return Err(fail("Namespace setup lock owner/protection is untrusted"));
        }
        let mut system = false;
        let mut admin = false;
        for index in 0..unsafe { (*actual.pointer()).AceCount } as u32 {
            let mut ace = null_mut();
            if unsafe { GetAce(actual.pointer(), index, &mut ace) } == 0 {
                return Err(last("Read namespace setup lock ACE"));
            }
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            let allow = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            if header.AceType != ALLOW_ACE || header.AceFlags != 0 || allow.Mask != MUTEX_ALL_ACCESS
            {
                return Err(fail("Namespace setup lock permissions are untrusted"));
            }
            match sid_text(addr_of!(allow.SidStart).cast_mut().cast())?.as_str() {
                SYSTEM if !system => system = true,
                ADMIN if !admin => admin = true,
                _ => return Err(fail("Namespace setup lock principal is untrusted")),
            }
        }
        if !system || !admin {
            return Err(fail(
                "Namespace setup lock is missing administrator/SYSTEM access",
            ));
        }
        match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self {
                handle,
                _thread_bound: PhantomData,
            }),
            WAIT_TIMEOUT => Err(WorkerError::new(
                "sandbox_busy",
                "Another administrator is provisioning this session's named-object namespace",
            )),
            _ => Err(last("Acquire administrator namespace setup lock")),
        }
    }
}
impl Drop for SetupLock {
    fn drop(&mut self) {
        if unsafe { ReleaseMutex(self.handle.as_raw_handle()) } == 0 {
            eprintln!(
                "Namespace setup lock release failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Retains exact before/after DACLs until the complete provisioning transaction
/// succeeds. Rollback refuses a detected intervening DACL edit. The setup mutex
/// coordinates this provisioner; unrelated administrator edits must be serialized
/// separately because Windows does not provide a DACL compare-and-swap here.
pub(super) struct Installation {
    _setup_lock: SetupLock,
    handle: OwnedHandle,
    original: Descriptor,
    applied: Descriptor,
    receipt: Receipt,
    active: bool,
}
impl Installation {
    pub(super) fn receipt(&self) -> Receipt {
        self.receipt.clone()
    }
    pub(super) fn commit(mut self) {
        self.active = false;
    }
    pub(super) fn rollback(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let current = Descriptor::read(self.handle.as_raw_handle())?;
        if current != self.applied {
            return Err(fail(
                "Named-object namespace changed after setup; original DACL was not overwritten and administrator recovery is required",
            ));
        }
        self.original.write(self.handle.as_raw_handle())?;
        if Descriptor::read(self.handle.as_raw_handle())? != self.original {
            return Err(fail(
                "Original named-object namespace DACL restoration is unconfirmed",
            ));
        }
        self.active = false;
        Ok(())
    }
}
impl Drop for Installation {
    fn drop(&mut self) {
        if let Err(error) = self.rollback() {
            eprintln!("Named-object namespace cleanup requires administrator recovery: {error}");
        }
    }
}

/// Called only by the explicit administrator provisioner with its new disabled
/// account. Grants do not inherit, do not touch BaseNamedObjects itself, and do
/// not confer READ_CONTROL/WRITE_DAC/WRITE_OWNER on the sandbox account.
pub(super) fn install(host: &str, account: &str) -> Result<Installation> {
    if host == account || [SYSTEM, ADMIN].contains(&host) || [SYSTEM, ADMIN].contains(&account) {
        return Err(fail(
            "Named-object namespace requires distinct individual host and sandbox SIDs",
        ));
    }
    let (session, path) = current_namespace()?;
    let setup_lock = SetupLock::acquire(session)?;
    let handle = open(&path, HOST_ACCESS | WRITE_DAC)?;
    let original = Descriptor::read(handle.as_raw_handle())?;
    original.verify_safety(session)?;
    let user_sid = parse_sid(account)?;
    let host_sid = parse_sid(host)?;
    let mut entries = Vec::new();
    for (principal, sid, mask) in [
        (account, user_sid.0, ACCOUNT_ACCESS),
        (host, host_sid.0, HOST_ACCESS),
    ] {
        if !original.exact_grant(principal, mask)? {
            entries.push(EXPLICIT_ACCESS_W {
                grfAccessPermissions: mask,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: NO_INHERITANCE,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_USER,
                    ptstrName: sid.cast(),
                },
            });
        }
    }
    let mut changed = null_mut();
    status(
        "Build account-specific named-object namespace grants",
        unsafe {
            SetEntriesInAclW(
                entries.len() as u32,
                entries.as_ptr(),
                original.pointer(),
                &mut changed,
            )
        },
    )?;
    let _changed = Local(changed.cast());
    let applied = Descriptor {
        owner: original.owner.clone(),
        protected: original.protected,
        acl: unsafe {
            std::slice::from_raw_parts(changed.cast::<u8>(), (*changed).AclSize as usize).to_vec()
        },
    };
    applied.verify(host, account)?;
    let receipt = Receipt {
        session_id: session,
        path,
        owner_sid: original.owner.clone(),
    };
    applied.write(handle.as_raw_handle())?;
    let mut installation = Installation {
        _setup_lock: setup_lock,
        handle,
        original,
        applied,
        receipt,
        active: true,
    };
    let readback = Descriptor::read(installation.handle.as_raw_handle());
    match readback {
        Ok(actual) if actual == installation.applied => Ok(installation),
        actual => {
            let reason = match actual {
                Ok(_) => "Named-object namespace setup readback did not match".to_owned(),
                Err(error) => error.to_string(),
            };
            let rollback = installation.rollback();
            Err(fail(format!("{reason}; namespace rollback: {rollback:?}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(extra: &str) -> Descriptor {
        // Existing session defaults include privileged Window Manager/SYSTEM
        // entries; ordinary accounts may create objects but must never be able
        // to rewrite or delete the shared namespace Directory itself.
        let sddl = format!(
            "O:SYD:P(A;;0x3;;;WD)(A;;0x20003;;;RC)(A;;0xf000f;;;S-1-5-90-0-1)(A;;0xf000f;;;SY){extra}"
        );
        let text: Vec<_> = sddl.encode_utf16().chain([0]).collect();
        let mut sd = null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &mut sd,
                    null_mut(),
                )
            },
            0
        );
        let _sd = Local(sd);
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = null_mut();
        assert_ne!(
            unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted) },
            0
        );
        assert_ne!(present, 0);
        Descriptor {
            owner: SYSTEM.into(),
            protected: true,
            acl: unsafe {
                std::slice::from_raw_parts(acl.cast::<u8>(), (*acl).AclSize as usize).to_vec()
            },
        }
    }

    #[test]
    fn rejects_shared_namespace_mutation_even_beside_exact_account_grants() {
        for unexpected in [
            "(A;;0x40000;;;WD)",
            "(A;;0x80000;;;BU)",
            "(A;;0x10000;;;AU)",
            "(A;;GA;;;S-1-5-21-1-2-3-1002)",
        ] {
            let sd = descriptor(&format!(
                "(A;;0xf;;;S-1-5-21-1-2-3-1001)(A;;0x20003;;;S-1-5-21-1-2-3-1003){unexpected}"
            ));
            assert!(
                sd.verify("S-1-5-21-1-2-3-1003", "S-1-5-21-1-2-3-1001")
                    .is_ok()
            );
            assert!(
                sd.verify_safety(1).is_err(),
                "Unexpected namespace mutation was accepted: {unexpected}"
            );
        }
    }

    #[test]
    fn independent_provisioned_accounts_and_hosts_can_coexist_without_mutation_authority() {
        let sd = descriptor(
            "(A;;0xf;;;S-1-5-21-1-2-3-1001)(A;;0xf;;;S-1-5-21-1-2-3-1002)(A;;0x20003;;;S-1-5-21-1-2-3-1003)(A;;0x20003;;;S-1-5-21-1-2-3-1004)",
        );
        sd.verify_safety(1).unwrap();
        sd.verify("S-1-5-21-1-2-3-1003", "S-1-5-21-1-2-3-1001")
            .unwrap();
        sd.verify("S-1-5-21-1-2-3-1004", "S-1-5-21-1-2-3-1002")
            .unwrap();
        assert!(
            sd.verify_safety(2).is_err(),
            "An unrelated session's Window Manager SID must not be trusted"
        );
    }
}
