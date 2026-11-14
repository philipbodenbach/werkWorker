//! Explicit administrator provisioning for the dedicated-account process tree.
//! Loading is read-only and fails closed: a JSON receipt is not proof of WFP policy.
use crate::core::{Result, WorkerError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    fs::{File, OpenOptions},
    io::{Read, Write},
    mem::{size_of, zeroed},
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::{
    Win32::{
        Foundation::*,
        NetworkManagement::{NetManagement::*, WindowsFilteringPlatform::*},
        Security::{Authorization::*, Cryptography::*, *},
        Storage::FileSystem::*,
        System::{Rpc::RPC_C_AUTHN_DEFAULT, Threading::*},
    },
    core::GUID,
};

const SCHEMA: u32 = 1;
// ACCESS_ALLOWED_ACE_TYPE from winnt.h; no object/callback ACE is accepted.
const ALLOW_ACE: u8 = 0;
const ADMIN: &str = "S-1-5-32-544";
const SYSTEM: &str = "S-1-5-18";
const SECRET_FILE: &str = "credential.dpapi";
const SANDBOX_RX: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
const SANDBOX_MODIFY: u32 = 0x1301bf;
const LAYERS: [GUID; 6] = [
    FWPM_LAYER_ALE_AUTH_CONNECT_V4,
    FWPM_LAYER_ALE_AUTH_CONNECT_V6,
    FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4,
    FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6,
    FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4,
    FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6,
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionRequest {
    pub provisioning_root: PathBuf,
    pub workspace: PathBuf,
    pub runner_source: PathBuf,
    pub host_sid: String,
    /// Explicit runtime trees copied into the new protected installation.
    pub toolchain_sources: Vec<PathBuf>,
    /// PATH/LIB/INCLUDE/LIBPATH/CARGO_HOME/RUSTUP_HOME/RUSTC only; templates
    /// {toolchain0}, {workspace}, {temp} resolve to the newly provisioned paths.
    pub toolchain_environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionManifest {
    pub schema_version: u32,
    pub host_sid: String,
    pub username: String,
    pub user_sid: String,
    pub workspace: PathBuf,
    pub private_temp: PathBuf,
    pub runner: PathBuf,
    pub runner_sha256: String,
    pub toolchain_roots: Vec<PathBuf>,
    pub toolchain_environment: BTreeMap<String, String>,
    pub network_sublayer_key: String,
    pub network_filter_keys: Vec<String>,
    pub credential_file: String,
}

pub struct Provisioning {
    pub manifest: ProvisionManifest,
    pub manifest_digest: String,
    credential: File,
    invocation_lock: File,
    crash_barrier: CrashBarrier,
    // Deny delete/write sharing on trusted files and delete sharing on ancestors.
    _guards: Vec<File>,
}

/// A successful normal drain removes this receipt. Process termination cannot
/// silently make a crashed invocation reusable merely by releasing its OS lock.
struct CrashBarrier {
    path: PathBuf,
    marker: std::sync::Mutex<Option<File>>,
}
impl CrashBarrier {
    fn begin(&self) -> Result<()> {
        let mut owned = self.marker.lock().unwrap();
        if owned.is_some() {
            return Err(WorkerError::new(
                "cleanup_required",
                "Invocation already started",
            ));
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(&self.path)
            .map_err(|error| WorkerError::new(
                "cleanup_required",
                format!("Cannot establish cleanup receipt {}: {error}. A surviving receipt requires explicit trusted recovery after all processes of the provisioned account are confirmed stopped", self.path.display()),
            ))?;
        // No payload or account credential is included in the crash receipt.
        file.write_all(b"{\"version\":1,\"cleanup\":\"unconfirmed\"}\n")?;
        file.sync_all()?;
        *owned = Some(file);
        Ok(())
    }
    fn confirm(&self) -> Result<()> {
        let mut owned = self.marker.lock().unwrap();
        if let Some(file) = owned.take() {
            drop(file);
            std::fs::remove_file(&self.path).map_err(|error| {
                WorkerError::new(
                    "cleanup_failed",
                    format!(
                        "Job ended, but cleanup receipt {} could not be removed: {error}",
                        self.path.display()
                    ),
                )
            })?;
        }
        Ok(())
    }
}

/// Contains a NUL-terminated password. Never Debug/Serialize/Clone this object.
pub struct SecretPassword(Vec<u16>);
impl SecretPassword {
    pub fn as_ptr(&self) -> *const u16 {
        self.0.as_ptr()
    }
}
impl Drop for SecretPassword {
    fn drop(&mut self) {
        for item in &mut self.0 {
            unsafe {
                std::ptr::write_volatile(item, 0);
            }
        }
    }
}
fn fail(message: impl Into<String>) -> WorkerError {
    WorkerError::new("sandbox_unavailable", message.into())
}
fn status(operation: &str, code: u32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(fail(format!("{operation}: Windows error {code:#x}")))
    }
}
fn last(operation: &str) -> WorkerError {
    fail(format!("{operation}: {}", std::io::Error::last_os_error()))
}
fn wide(value: impl AsRef<std::ffi::OsStr>) -> Result<Vec<u16>> {
    let mut v: Vec<u16> = value.as_ref().encode_wide().collect();
    if v.contains(&0) {
        return Err(fail("NUL in provisioning input"));
    }
    v.push(0);
    Ok(v)
}
struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn descriptor(sddl: &str) -> Result<Local> {
    let mut value = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(sddl)?.as_ptr(),
            1,
            &mut value,
            null_mut(),
        )
    } == 0
    {
        return Err(last("Parse provisioning ACL"));
    }
    Ok(Local(value))
}
fn sid_text(sid: PSID) -> Result<String> {
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(fail("Invalid account SID"));
    }
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(last("Format account SID"));
    }
    let _memory = Local(text.cast());
    let mut len = 0;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
    }
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(text, len)
    }))
}
fn parse_sid(value: &str) -> Result<Local> {
    let mut sid = null_mut();
    if unsafe { ConvertStringSidToSidW(wide(value)?.as_ptr(), &mut sid) } == 0 {
        return Err(last("Parse account SID"));
    }
    Ok(Local(sid))
}
fn current_sid() -> Result<String> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last("Open host identity"));
    }
    let result = (|| {
        let mut bytes = 0;
        unsafe {
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut bytes);
        }
        if bytes == 0 || bytes > 65536 {
            return Err(fail("Invalid host token information size"));
        }
        let mut storage = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                storage.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            )
        } == 0
        {
            return Err(last("Read host identity"));
        }
        sid_text(unsafe { (*storage.as_ptr().cast::<TOKEN_USER>()).User.Sid })
    })();
    unsafe {
        CloseHandle(token);
    }
    result
}
/// Checked before input reads or mutations by the setup helper.
pub fn require_administrator() -> Result<()> {
    let admin = parse_sid(ADMIN)?;
    let mut member = 0;
    if unsafe { CheckTokenMembership(null_mut(), admin.0, &mut member) } == 0 {
        return Err(last("Check administrator token"));
    }
    if member == 0 {
        return Err(WorkerError::new(
            "admin_required",
            "Explicit elevated administrator setup is required; no provisioning was performed",
        ));
    }
    Ok(())
}
fn account_sid(name: &str) -> Result<String> {
    let name = wide(name)?;
    let mut size = 0;
    let mut domain_size = 0;
    let mut usage = 0;
    unsafe {
        LookupAccountNameW(
            null(),
            name.as_ptr(),
            null_mut(),
            &mut size,
            null_mut(),
            &mut domain_size,
            &mut usage,
        );
    }
    if size == 0 || size > 65536 || domain_size > 65536 {
        return Err(last("Find provisioned account"));
    }
    let mut sid = vec![0u8; size as usize];
    let mut domain = vec![0u16; domain_size as usize];
    if unsafe {
        LookupAccountNameW(
            null(),
            name.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut size,
            domain.as_mut_ptr(),
            &mut domain_size,
            &mut usage,
        )
    } == 0
    {
        return Err(last("Resolve provisioned account"));
    }
    sid_text(sid.as_mut_ptr().cast())
}
fn validate_account(manifest: &ProvisionManifest) -> Result<()> {
    if !manifest.username.starts_with("wwbox_")
        || manifest.username.len() != 18
        || account_sid(&manifest.username)? != manifest.user_sid
    {
        return Err(fail("Provisioned account identity changed"));
    }
    let name = wide(&manifest.username)?;
    let mut info = null_mut();
    status("Inspect local sandbox account", unsafe {
        NetUserGetInfo(null(), name.as_ptr(), 1, &mut info)
    })?;
    if info.is_null() {
        return Err(fail("Empty local account record"));
    }
    let user = unsafe { &*info.cast::<USER_INFO_1>() };
    let okay = user.usri1_priv == USER_PRIV_USER
        && user.usri1_flags & (UF_ACCOUNTDISABLE | UF_PASSWD_NOTREQD) == 0
        && user.usri1_flags & UF_NORMAL_ACCOUNT != 0;
    unsafe {
        NetApiBufferFree(info.cast());
    }
    if !okay {
        return Err(fail(
            "Sandbox account is disabled, privileged, or has no password requirement",
        ));
    }
    let mut groups = null_mut();
    let mut read = 0;
    let mut total = 0;
    status("Inspect sandbox group memberships", unsafe {
        NetUserGetLocalGroups(
            null(),
            name.as_ptr(),
            0,
            LG_INCLUDE_INDIRECT,
            &mut groups,
            MAX_PREFERRED_LENGTH,
            &mut read,
            &mut total,
        )
    })?;
    let result = (|| {
        if read != total || read > 1024 {
            return Err(fail("Incomplete sandbox group inspection"));
        }
        if read == 0 {
            return Ok(());
        }
        if groups.is_null() {
            return Err(fail("Empty group membership buffer"));
        }
        for group in unsafe {
            std::slice::from_raw_parts(groups.cast::<LOCALGROUP_USERS_INFO_0>(), read as usize)
        } {
            let mut len = 0;
            unsafe {
                while *group.lgrui0_name.add(len) != 0 {
                    len += 1;
                }
            }
            let name = String::from_utf16_lossy(unsafe {
                std::slice::from_raw_parts(group.lgrui0_name, len)
            });
            if account_sid(&name)? != "S-1-5-32-545" {
                return Err(fail("Sandbox account belongs to an unexpected local group"));
            }
        }
        Ok(())
    })();
    unsafe {
        NetApiBufferFree(groups.cast());
    }
    result
}
fn trusted_acl(handle: HANDLE, host_sid: &str, sandbox: Option<(&str, u32)>) -> Result<()> {
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut sd = null_mut();
    status("Read trusted provisioning ACL", unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    })?;
    let _sd = Local(sd);
    if ![host_sid, ADMIN, SYSTEM].contains(&sid_text(owner)?.as_str()) || dacl.is_null() {
        return Err(fail("Provisioning owner/DACL is not trusted"));
    }
    if unsafe { (*dacl).AceCount } == 0 {
        return Err(fail("Provisioning ACL is empty"));
    }
    let mut sandbox_mask = 0;
    for index in 0..unsafe { (*dacl).AceCount } as u32 {
        let mut ace = null_mut();
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            return Err(last("Read provisioning ACE"));
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if header.AceType != ALLOW_ACE {
            return Err(fail("Unexpected provisioning ACE type"));
        }
        let allow = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = std::ptr::addr_of!(allow.SidStart).cast_mut().cast();
        let principal = sid_text(sid)?;
        if ![host_sid, ADMIN, SYSTEM].contains(&principal.as_str()) {
            let (expected_sid, allowed) =
                sandbox.ok_or_else(|| fail("Provisioning assets grant unexpected access"))?;
            if principal != expected_sid || allow.Mask & !allowed != 0 {
                return Err(fail("Provisioning assets grant unexpected access"));
            }
            if header.AceFlags & INHERIT_ONLY_ACE as u8 == 0 {
                sandbox_mask |= allow.Mask;
            }
        }
    }
    if sandbox.is_some_and(|(_, expected)| sandbox_mask != expected) {
        return Err(fail(
            "Provisioned sandbox access grant is missing or incomplete",
        ));
    }
    Ok(())
}
fn pin_chain(path: &Path, guards: &mut Vec<File>) -> Result<PathBuf> {
    let text = path.as_os_str().to_string_lossy();
    let drive = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let bytes = drive.as_bytes();
    if !path.is_absolute()
        || bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'\\'
    {
        return Err(fail(
            "Provisioning requires an ordinary or extended local drive path",
        ));
    }
    let mut paths: Vec<_> = path.ancestors().collect();
    paths.reverse();
    for item in paths {
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(item)?;
        if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(fail("Reparse points are forbidden in provisioning paths"));
        }
        guards.push(file);
    }
    Ok(std::fs::canonicalize(path)?)
}
fn trusted_file(
    path: &Path,
    host: &str,
    sandbox: Option<(&str, u32)>,
    guards: &mut Vec<File>,
) -> Result<File> {
    pin_chain(
        path.parent()
            .ok_or_else(|| fail("Missing provisioning parent"))?,
        guards,
    )?;
    let file = OpenOptions::new()
        .access_mode(GENERIC_READ | READ_CONTROL)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if !file.metadata()?.is_file()
        || file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(fail("Trusted provisioning asset is not an ordinary file"));
    }
    trusted_acl(file.as_raw_handle(), host, sandbox)?;
    Ok(file)
}
fn pin_directory(
    path: &Path,
    host: &str,
    sandbox: Option<(&str, u32)>,
    guards: &mut Vec<File>,
) -> Result<PathBuf> {
    let canonical = pin_chain(path, guards)?;
    let file = guards
        .last()
        .ok_or_else(|| fail("Missing pinned directory"))?;
    if !file.metadata()?.is_dir() {
        return Err(fail("Expected a provisioned directory"));
    }
    trusted_acl(file.as_raw_handle(), host, sandbox)?;
    Ok(canonical)
}
fn guid(text: &str) -> Result<GUID> {
    Ok(GUID::from_u128(
        uuid::Uuid::parse_str(text)
            .map_err(|_| fail("Invalid network policy GUID"))?
            .as_u128(),
    ))
}
fn same_guid(a: GUID, b: GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}
struct Engine(HANDLE);
impl Engine {
    fn open() -> Result<Self> {
        let mut handle = null_mut();
        let mut session: FWPM_SESSION0 = unsafe { zeroed() };
        session.txnWaitTimeoutInMSec = 5000;
        status("Open WFP engine (query permission required)", unsafe {
            FwpmEngineOpen0(
                null(),
                RPC_C_AUTHN_DEFAULT as u32,
                null(),
                &session,
                &mut handle,
            )
        })?;
        Ok(Self(handle))
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            FwpmEngineClose0(self.0);
        }
    }
}
fn user_condition(user_sid: &str) -> Result<Local> {
    descriptor(&format!("D:(A;;0x1;;;{user_sid})"))
}
fn verify_network(manifest: &ProvisionManifest) -> Result<()> {
    if manifest.network_filter_keys.len() != LAYERS.len() {
        return Err(fail("Incomplete network filter receipt"));
    }
    let engine = Engine::open()?;
    let sublayer = guid(&manifest.network_sublayer_key)?;
    let mut actual_sublayer = null_mut();
    status("Read required network sublayer", unsafe {
        FwpmSubLayerGetByKey0(engine.0, &sublayer, &mut actual_sublayer)
    })?;
    let sublayer_okay = !actual_sublayer.is_null()
        && unsafe {
            same_guid((*actual_sublayer).subLayerKey, sublayer)
                && (*actual_sublayer).flags == FWPM_SUBLAYER_FLAG_PERSISTENT
                && (*actual_sublayer).weight == u16::MAX
        };
    unsafe {
        FwpmFreeMemory0((&mut actual_sublayer as *mut *mut FWPM_SUBLAYER0).cast());
    }
    if !sublayer_okay {
        return Err(fail("Required network sublayer changed"));
    }
    let expected_sd = user_condition(&manifest.user_sid)?;
    let expected_size = unsafe { GetSecurityDescriptorLength(expected_sd.0) } as usize;
    let expected_bytes =
        unsafe { std::slice::from_raw_parts(expected_sd.0.cast::<u8>(), expected_size) };
    for (text, layer) in manifest.network_filter_keys.iter().zip(LAYERS) {
        let key = guid(text)?;
        let mut filter = null_mut();
        status("Read required account network deny", unsafe {
            FwpmFilterGetByKey0(engine.0, &key, &mut filter)
        })?;
        let result = (|| {
            if filter.is_null() {
                return Err(fail("Empty WFP filter"));
            }
            let f = unsafe { &*filter };
            if !same_guid(f.filterKey, key)
                || !same_guid(f.layerKey, layer)
                || !same_guid(f.subLayerKey, sublayer)
                || f.flags != FWPM_FILTER_FLAG_PERSISTENT
                || f.weight.r#type != FWP_UINT8
                || unsafe { f.weight.Anonymous.uint8 } != 15
                || f.action.r#type != FWP_ACTION_BLOCK
                || f.numFilterConditions != 1
                || f.filterCondition.is_null()
            {
                return Err(fail("Required WFP deny filter changed"));
            }
            let c = unsafe { &*f.filterCondition };
            if !same_guid(c.fieldKey, FWPM_CONDITION_ALE_USER_ID)
                || c.matchType != FWP_MATCH_EQUAL
                || c.conditionValue.r#type != FWP_SECURITY_DESCRIPTOR_TYPE
            {
                return Err(fail("Network deny is not an unconditional account match"));
            }
            let blob = unsafe { c.conditionValue.Anonymous.sd };
            if blob.is_null()
                || unsafe { (*blob).size as usize } != expected_size
                || unsafe { (*blob).data }.is_null()
            {
                return Err(fail("Network account descriptor changed"));
            }
            if unsafe { std::slice::from_raw_parts((*blob).data, (*blob).size as usize) }
                != expected_bytes
            {
                return Err(fail("Network deny account identity changed"));
            }
            Ok(())
        })();
        unsafe {
            FwpmFreeMemory0((&mut filter as *mut *mut FWPM_FILTER0).cast());
        }
        result?;
    }
    Ok(())
}
fn validate_environment(manifest: &ProvisionManifest) -> Result<()> {
    for (name, value) in &manifest.toolchain_environment {
        if ![
            "PATH",
            "LIB",
            "INCLUDE",
            "LIBPATH",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "RUSTC",
        ]
        .contains(&name.as_str())
            || value.is_empty()
            || value.contains('\0')
        {
            return Err(fail("Unexpected toolchain environment entry"));
        }
        for part in value.split(';') {
            let path = Path::new(part);
            if !path.is_absolute()
                || !manifest
                    .toolchain_roots
                    .iter()
                    .chain([&manifest.workspace, &manifest.private_temp])
                    .any(|root| path.starts_with(root))
            {
                return Err(fail(format!(
                    "Toolchain environment {name} escapes provisioned roots"
                )));
            }
            if part.contains("..") {
                return Err(fail("Relative components in toolchain environment"));
            }
        }
    }
    Ok(())
}
/// Pin and inspect the exact configured provisioning state before every launch.
pub fn load(path: &Path, workspace: &Path) -> Result<Provisioning> {
    let host = current_sid()?;
    let mut guards = Vec::new();
    let provisioning_root = path
        .parent()
        .ok_or_else(|| fail("Missing provisioning directory"))?;
    pin_directory(provisioning_root, &host, None, &mut guards)?;
    let mut manifest_file = trusted_file(path, &host, None, &mut guards)?;
    if manifest_file.metadata()?.len() > 1024 * 1024 {
        return Err(fail("Oversized provisioning manifest"));
    }
    let mut bytes = Vec::new();
    manifest_file.read_to_end(&mut bytes)?;
    let manifest: ProvisionManifest = serde_json::from_slice(&bytes)
        .map_err(|e| fail(format!("Invalid provisioning manifest: {e}")))?;
    if manifest.schema_version != SCHEMA
        || manifest.host_sid != host
        || manifest.credential_file != SECRET_FILE
    {
        return Err(fail(
            "Provisioning version/host/credential identity mismatch",
        ));
    }
    let pinned_workspace = pin_chain(workspace, &mut guards)?;
    if pin_directory(
        &manifest.workspace,
        &host,
        Some((&manifest.user_sid, SANDBOX_MODIFY)),
        &mut guards,
    )? != pinned_workspace
    {
        return Err(fail("Provisioning belongs to a different workspace"));
    }
    pin_directory(
        &manifest.private_temp,
        &host,
        Some((&manifest.user_sid, SANDBOX_MODIFY)),
        &mut guards,
    )?;
    for toolchain in &manifest.toolchain_roots {
        pin_directory(
            toolchain,
            &host,
            Some((&manifest.user_sid, SANDBOX_RX)),
            &mut guards,
        )?;
    }
    validate_environment(&manifest)?;
    let mut runner = trusted_file(
        &manifest.runner,
        &host,
        Some((&manifest.user_sid, SANDBOX_RX)),
        &mut guards,
    )?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = runner.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    if format!("{:x}", hash.finalize()) != manifest.runner_sha256 {
        return Err(fail("Provisioned runner content changed"));
    }
    guards.push(runner);
    guards.push(manifest_file);
    let credential = trusted_file(
        &path
            .parent()
            .ok_or_else(|| fail("Missing provisioning directory"))?
            .join(SECRET_FILE),
        &host,
        None,
        &mut guards,
    )?;
    if credential.metadata()?.len() > 65536 {
        return Err(fail("Oversized credential blob"));
    }
    let invocation_lock = OpenOptions::new()
        .access_mode(GENERIC_READ | GENERIC_WRITE | READ_CONTROL)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(provisioning_root.join("invocation.lock"))?;
    if !invocation_lock.metadata()?.is_file()
        || invocation_lock.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(fail("Invocation lock is not an ordinary protected file"));
    }
    trusted_acl(invocation_lock.as_raw_handle(), &host, None)?;
    validate_account(&manifest)?;
    verify_network(&manifest)?;
    Ok(Provisioning {
        manifest,
        manifest_digest: format!("{:x}", Sha256::digest(&bytes)),
        credential,
        invocation_lock,
        crash_barrier: CrashBarrier {
            path: provisioning_root.join("incomplete-run.json"),
            marker: std::sync::Mutex::new(None),
        },
        _guards: guards,
    })
}
impl Provisioning {
    /// The owned lock remains held until this Provisioning is dropped after cleanup.
    pub fn try_lock_invocation(&self) -> Result<()> {
        fs2::FileExt::try_lock_exclusive(&self.invocation_lock).map_err(|error| {
            WorkerError::new(
                "sandbox_busy",
                format!("Provisioned workspace is already active or cannot be locked: {error}"),
            )
        })
    }
    /// Call while holding the invocation lock, immediately before first spawn.
    pub fn begin_invocation(&self) -> Result<()> {
        self.crash_barrier.begin()
    }
    /// Only the trusted supervisor may call this after confirmed whole-Job drain.
    pub fn confirm_cleanup(&self) -> Result<()> {
        self.crash_barrier.confirm()
    }
    pub fn password(&self) -> Result<SecretPassword> {
        use std::os::windows::fs::FileExt;
        let mut bytes = vec![0u8; self.credential.metadata()?.len() as usize];
        let mut offset = 0;
        while offset < bytes.len() {
            let n = self
                .credential
                .seek_read(&mut bytes[offset..], offset as u64)?;
            if n == 0 {
                return Err(fail("Truncated credential blob"));
            }
            offset += n;
        }
        let input = CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_mut_ptr(),
        };
        let mut output: CRYPT_INTEGER_BLOB = unsafe { zeroed() };
        if unsafe {
            CryptUnprotectData(
                &input,
                null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } == 0
        {
            return Err(last("Decrypt protected sandbox credential"));
        }
        let memory = Local(output.pbData.cast());
        if output.pbData.is_null() {
            return Err(fail("Empty DPAPI output"));
        }
        let decrypted =
            unsafe { std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize) };
        let result = if decrypted.is_empty() || decrypted.len() > 1024 || decrypted.len() % 2 != 0 {
            Err(fail("Invalid protected credential"))
        } else {
            let mut password: Vec<u16> = decrypted
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes(*b))
                .collect();
            if password.contains(&0) {
                Err(fail("Invalid NUL in protected credential"))
            } else {
                password.push(0);
                Ok(SecretPassword(password))
            }
        };
        for byte in decrypted {
            unsafe {
                std::ptr::write_volatile(byte, 0);
            }
        }
        drop(memory);
        result
    }
}
fn directory(path: &Path, sddl: &str) -> Result<()> {
    let sd = descriptor(sddl)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    if unsafe { CreateDirectoryW(wide(path)?.as_ptr(), &attributes) } == 0 {
        return Err(last("Create fresh provisioning directory"));
    }
    Ok(())
}
fn set_acl(path: &Path, sddl: &str) -> Result<()> {
    let sd = descriptor(sddl)?;
    let mut dacl = null_mut();
    let mut present = 0;
    let mut defaulted = 0;
    let mut owner = null_mut();
    if unsafe { GetSecurityDescriptorOwner(sd.0, &mut owner, &mut defaulted) } == 0 {
        return Err(last("Read setup ACL owner"));
    }
    if unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) } == 0
        || present == 0
    {
        return Err(fail("Invalid setup ACL"));
    }
    status("Set exact setup ACL", unsafe {
        SetNamedSecurityInfoW(
            wide(path)?.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION
                | OWNER_SECURITY_INFORMATION,
            owner,
            null_mut(),
            dacl,
            null(),
        )
    })
}
fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    // Open first without following reparse points, and prohibit source writes or
    // replacement while copying. Directory handles stay live throughout recursion.
    let mut input = OpenOptions::new()
        .access_mode(GENERIC_READ | READ_CONTROL)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(source)?;
    let metadata = input.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(fail("Toolchain copies may not contain reparse points"));
    }
    if metadata.is_dir() {
        std::fs::create_dir(target)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
    } else {
        return Err(fail("Unsupported toolchain entry"));
    }
    Ok(())
}
fn install_network(manifest: &ProvisionManifest) -> Result<()> {
    let engine = Engine::open()?;
    status("Begin network policy transaction", unsafe {
        FwpmTransactionBegin0(engine.0, 0)
    })?;
    let result = (|| {
        let name = wide(format!("werkWorker {} network deny", manifest.username))?;
        let key = guid(&manifest.network_sublayer_key)?;
        let sd = descriptor(&format!(
            "O:BAG:BAD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;{})",
            manifest.host_sid
        ))?;
        let sublayer = FWPM_SUBLAYER0 {
            subLayerKey: key,
            displayData: FWPM_DISPLAY_DATA0 {
                name: name.as_ptr().cast_mut(),
                description: null_mut(),
            },
            flags: FWPM_SUBLAYER_FLAG_PERSISTENT,
            providerKey: null_mut(),
            providerData: FWP_BYTE_BLOB {
                size: 0,
                data: null_mut(),
            },
            weight: u16::MAX,
        };
        status("Install persistent network sublayer", unsafe {
            FwpmSubLayerAdd0(engine.0, &sublayer, sd.0)
        })?;
        let user = user_condition(&manifest.user_sid)?;
        let mut blob = FWP_BYTE_BLOB {
            size: unsafe { GetSecurityDescriptorLength(user.0) },
            data: user.0.cast(),
        };
        let mut condition = FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_ALE_USER_ID,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 { sd: &mut blob },
            },
        };
        for (filter_key, layer) in manifest.network_filter_keys.iter().zip(LAYERS) {
            let mut filter: FWPM_FILTER0 = unsafe { zeroed() };
            filter.filterKey = guid(filter_key)?;
            filter.displayData.name = name.as_ptr().cast_mut();
            filter.flags = FWPM_FILTER_FLAG_PERSISTENT;
            filter.layerKey = layer;
            filter.subLayerKey = key;
            filter.weight.r#type = FWP_UINT8;
            filter.weight.Anonymous.uint8 = 15;
            filter.numFilterConditions = 1;
            filter.filterCondition = &mut condition;
            filter.action.r#type = FWP_ACTION_BLOCK;
            status("Install persistent account network deny", unsafe {
                FwpmFilterAdd0(engine.0, &filter, sd.0, null_mut())
            })?;
        }
        status("Commit account network deny", unsafe {
            FwpmTransactionCommit0(engine.0)
        })
    })();
    if result.is_err() {
        unsafe {
            FwpmTransactionAbort0(engine.0);
        }
    }
    result?;
    verify_network(manifest)
}
fn enable_account(username: &str, enabled: bool) -> Result<()> {
    let flags = USER_INFO_1008 {
        usri1008_flags: UF_NORMAL_ACCOUNT
            | UF_SCRIPT
            | UF_DONT_EXPIRE_PASSWD
            | if enabled { 0 } else { UF_ACCOUNTDISABLE },
    };
    status("Set sandbox account enabled state", unsafe {
        NetUserSetInfo(
            null(),
            wide(username)?.as_ptr(),
            1008,
            (&flags as *const USER_INFO_1008).cast(),
            null_mut(),
        )
    })
}
/// Explicit administrator operation. The account stays disabled until all assets
/// and persistent network denies are ready. A failed setup retains a disabled
/// account and protected diagnostics; no failed receipt can pass `load`.
pub fn provision(request: ProvisionRequest) -> Result<PathBuf> {
    require_administrator()?;
    let host = parse_sid(&request.host_sid)?;
    drop(host);
    if request.host_sid == ADMIN || request.host_sid == SYSTEM {
        return Err(fail(
            "Setup requires the individual unelevated host user's SID",
        ));
    }
    if request.provisioning_root.exists() || request.workspace.exists() {
        return Err(fail("Provisioning root and workspace must both be new"));
    }
    let mut guards = Vec::new();
    for path in [&request.provisioning_root, &request.workspace] {
        pin_chain(
            path.parent()
                .ok_or_else(|| fail("Setup path has no parent"))?,
            &mut guards,
        )?;
    }
    let source_runner = pin_chain(&request.runner_source, &mut guards)?;
    if !source_runner.is_file() {
        return Err(fail("Runner source must be a native executable file"));
    }
    for source in &request.toolchain_sources {
        pin_chain(source, &mut guards)?;
    }
    let trusted = format!(
        "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{})",
        request.host_sid
    );
    directory(&request.provisioning_root, &trusted)?;
    let username = format!("wwbox_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let mut random = [0u8; 48];
    if unsafe {
        BCryptGenRandom(
            null_mut(),
            random.as_mut_ptr(),
            random.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    } < 0
    {
        return Err(last("Generate sandbox password"));
    }
    let mut password = SecretPassword(
        "Ww!9"
            .encode_utf16()
            .chain(random.iter().map(|b| b'a' as u16 + (b % 26) as u16))
            .chain([0])
            .collect(),
    );
    random.fill(0);
    let name = wide(&username)?;
    let comment = wide(
        "werkWorker dedicated offline sandbox account; managed by explicit administrator setup",
    )?;
    let info = USER_INFO_1 {
        usri1_name: name.as_ptr().cast_mut(),
        usri1_password: password.0.as_mut_ptr(),
        usri1_priv: USER_PRIV_USER,
        usri1_flags: UF_NORMAL_ACCOUNT | UF_SCRIPT | UF_ACCOUNTDISABLE | UF_DONT_EXPIRE_PASSWD,
        usri1_comment: comment.as_ptr().cast_mut(),
        ..unsafe { zeroed() }
    };
    status("Create disabled sandbox account", unsafe {
        NetUserAdd(null(), 1, (&info as *const USER_INFO_1).cast(), null_mut())
    })?;
    let result = (|| {
        let user_sid = account_sid(&username)?;
        let private_temp = request.provisioning_root.join("private-temp");
        let writable = format!(
            "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{})(A;OICI;0x1301bf;;;{user_sid})",
            request.host_sid
        );
        directory(&request.workspace, &writable)?;
        directory(&private_temp, &writable)?;
        let runner = request.provisioning_root.join("ww-windows-runner.exe");
        copy_tree(&source_runner, &runner)?;
        let runner_sha256 = format!("{:x}", Sha256::digest(std::fs::read(&runner)?));
        // The trusted runner must be executable by the account; separate file ACL
        // keeps credential/manifest inaccessible and blocks runner modification.
        let executable = format!(
            "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{})(A;OICI;FRFX;;;{user_sid})",
            request.host_sid
        );
        set_acl(&runner, &executable)?;
        let toolchain_dir = request.provisioning_root.join("toolchains");
        directory(&toolchain_dir, &trusted)?;
        let mut toolchain_roots = Vec::new();
        for (index, source) in request.toolchain_sources.iter().enumerate() {
            let target = toolchain_dir.join(index.to_string());
            copy_tree(source, &target)?;
            set_acl(&target, &executable)?;
            toolchain_roots.push(target);
        }
        let mut environment = request.toolchain_environment.clone();
        for value in environment.values_mut() {
            *value = value
                .replace("{workspace}", &request.workspace.to_string_lossy())
                .replace("{temp}", &private_temp.to_string_lossy());
            for (index, root) in toolchain_roots.iter().enumerate() {
                *value = value.replace(&format!("{{toolchain{index}}}"), &root.to_string_lossy());
            }
        }
        let manifest = ProvisionManifest {
            schema_version: SCHEMA,
            host_sid: request.host_sid.clone(),
            username: username.clone(),
            user_sid,
            workspace: request.workspace.clone(),
            private_temp,
            runner,
            runner_sha256,
            toolchain_roots,
            toolchain_environment: environment,
            network_sublayer_key: uuid::Uuid::new_v4().to_string(),
            network_filter_keys: (0..LAYERS.len())
                .map(|_| uuid::Uuid::new_v4().to_string())
                .collect(),
            credential_file: SECRET_FILE.into(),
        };
        validate_environment(&manifest)?;
        let mut plaintext: Vec<u8> = password.0[..password.0.len() - 1]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let input = CRYPT_INTEGER_BLOB {
            cbData: plaintext.len() as u32,
            pbData: plaintext.as_mut_ptr(),
        };
        let mut output: CRYPT_INTEGER_BLOB = unsafe { zeroed() };
        let encrypted = unsafe {
            CryptProtectData(
                &input,
                null(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        for byte in &mut plaintext {
            unsafe {
                std::ptr::write_volatile(byte, 0);
            }
        }
        if encrypted == 0 {
            return Err(last("Protect sandbox credential"));
        }
        let memory = Local(output.pbData.cast());
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(request.provisioning_root.join(SECRET_FILE))?
            .write_all(unsafe {
                std::slice::from_raw_parts(output.pbData, output.cbData as usize)
            })?;
        drop(memory);
        install_network(&manifest)?;
        let lock_path = request.provisioning_root.join("invocation.lock");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)?
            .sync_all()?;
        set_acl(&lock_path, &trusted)?;
        let path = request.provisioning_root.join("provisioning.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(&serde_json::to_vec_pretty(&manifest).map_err(|e| fail(e.to_string()))?)?;
        file.sync_all()?;
        set_acl(&path, &trusted)?;
        set_acl(&request.provisioning_root.join(SECRET_FILE), &trusted)?;
        enable_account(&username, true)?;
        Ok(path)
    })();
    if let Err(error) = result {
        let disabled = enable_account(&username, false);
        return Err(fail(format!(
            "Provisioning incomplete for {username}; retained root {}. Account disable result: {}. {error}",
            request.provisioning_root.display(),
            if disabled.is_ok() {
                "confirmed"
            } else {
                "FAILED; administrator cleanup required"
            }
        )));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_missing_wfp_policy_fails_closed() {
        // Read-only: no filters, accounts, firewall rules, or ACLs are created.
        let engine = Engine::open().unwrap();
        let key = guid(&uuid::Uuid::new_v4().to_string()).unwrap();
        let mut filter = null_mut();
        assert_eq!(
            unsafe { FwpmFilterGetByKey0(engine.0, &key, &mut filter) },
            FWP_E_FILTER_NOT_FOUND as u32
        );
        assert!(filter.is_null());
        let manifest = ProvisionManifest {
            schema_version: 1,
            host_sid: current_sid().unwrap(),
            username: String::new(),
            user_sid: "S-1-1-0".into(),
            workspace: PathBuf::new(),
            private_temp: PathBuf::new(),
            runner: PathBuf::new(),
            runner_sha256: String::new(),
            toolchain_roots: Vec::new(),
            toolchain_environment: BTreeMap::new(),
            network_sublayer_key: uuid::Uuid::new_v4().to_string(),
            network_filter_keys: LAYERS
                .iter()
                .map(|_| uuid::Uuid::new_v4().to_string())
                .collect(),
            credential_file: String::new(),
        };
        let error = verify_network(&manifest).unwrap_err();
        assert!(
            error.to_string().contains("Read required network sublayer"),
            "{error}"
        );
    }

    #[test]
    fn crash_receipt_survives_owner_drop_and_requires_confirmed_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("incomplete-run.json");
        let barrier = || CrashBarrier {
            path: path.clone(),
            marker: std::sync::Mutex::new(None),
        };
        let first = barrier();
        first.begin().unwrap();
        assert_eq!(barrier().begin().unwrap_err().code, "cleanup_required");
        first.confirm().unwrap();
        assert!(!path.exists());
        let interrupted = barrier();
        interrupted.begin().unwrap();
        drop(interrupted);
        assert!(path.exists());
        assert_eq!(barrier().begin().unwrap_err().code, "cleanup_required");
    }

    #[test]
    fn pins_extended_local_paths_and_rejects_devices() {
        let directory = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(directory.path()).unwrap();
        let mut guards = Vec::new();
        assert_eq!(pin_chain(&canonical, &mut guards).unwrap(), canonical);
        assert!(pin_chain(Path::new(r"\\.\NUL"), &mut Vec::new()).is_err());
        assert!(pin_chain(Path::new(r"\\?\UNC\localhost\share"), &mut Vec::new()).is_err());
        assert!(pin_chain(Path::new(r"relative\path"), &mut Vec::new()).is_err());
    }

    #[test]
    fn native_acl_rejects_sandbox_write_to_trusted_assets() {
        let directory = tempfile::tempdir().unwrap();
        let host = current_sid().unwrap();
        for (name, grant, expected) in [("reader", "FRFX", true), ("writer", "FRFXFW", false)] {
            let path = directory.path().join(name);
            std::fs::write(&path, b"synthetic ACL fixture").unwrap();
            set_acl(
                &path,
                &format!("O:{host}D:P(A;;FA;;;{host})(A;;{grant};;;WD)"),
            )
            .unwrap();
            let file = OpenOptions::new()
                .access_mode(READ_CONTROL | GENERIC_READ)
                .share_mode(FILE_SHARE_READ)
                .open(&path)
                .unwrap();
            assert_eq!(
                trusted_acl(file.as_raw_handle(), &host, Some(("S-1-1-0", SANDBOX_RX))).is_ok(),
                expected
            );
            assert!(trusted_acl(file.as_raw_handle(), &host, None).is_err());
        }
    }
}
