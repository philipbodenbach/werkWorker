//! Handle-relative local NTFS access. Directory handles deny delete sharing,
//! preventing ancestor rename/replacement for their entire retained lifetime.
use super::*;
use std::ffi::{OsString, c_void};
use std::mem::{offset_of, size_of};
use std::os::windows::{
    ffi::{OsStrExt, OsStringExt},
    io::{AsRawHandle, FromRawHandle},
};
use windows_sys::Win32::{
    Foundation::{ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE, UNICODE_STRING},
    Storage::FileSystem::*,
    System::IO::IO_STATUS_BLOCK,
};

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: HANDLE,
    object_name: *mut UNICODE_STRING,
    attributes: u32,
    security_descriptor: *mut c_void,
    security_quality_of_service: *mut c_void,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        handle: *mut HANDLE,
        access: u32,
        attributes: *const ObjectAttributes,
        status: *mut IO_STATUS_BLOCK,
        allocation: *const i64,
        file_attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const c_void,
        ea_length: u32,
    ) -> i32;
    fn NtSetInformationFile(
        handle: HANDLE,
        status: *mut IO_STATUS_BLOCK,
        information: *const c_void,
        length: u32,
        class: u32,
    ) -> i32;
}

fn denied() -> WorkerError {
    WorkerError::new(
        "path_denied",
        "Unsafe or inaccessible native workspace path",
    )
}

fn native_error(operation: &str) -> WorkerError {
    WorkerError::new(
        "path_denied",
        format!(
            "{operation}: Windows error {}",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        ),
    )
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Reject Win32/NT aliases before the filesystem parser sees a component.
fn validate_component(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c < ' ' || matches!(c, ':' | '/' | '\\' | '<' | '>' | '"' | '|' | '?' | '*'))
        || name.encode_utf16().count() > 255
    {
        return Err(denied());
    }
    let base = name.split('.').next().unwrap().to_uppercase();
    if matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || base
        .strip_prefix("COM")
        .or_else(|| base.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    {
        return Err(denied());
    }
    Ok(())
}

fn information<T: Default>(file: &File, class: FILE_INFO_BY_HANDLE_CLASS) -> Result<T> {
    let mut value = T::default();
    // SAFETY: Windows writes a fixed-size information structure into live storage.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            class,
            (&mut value as *mut T).cast(),
            size_of::<T>() as u32,
        )
    } == 0
    {
        return Err(denied());
    }
    Ok(value)
}

fn attributes(file: &File) -> Result<FILE_ATTRIBUTE_TAG_INFO> {
    let value: FILE_ATTRIBUTE_TAG_INFO = information(file, FileAttributeTagInfo)?;
    if value.FileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DEVICE) != 0 {
        return Err(denied());
    }
    Ok(value)
}

fn delete_on_close(file: &File) {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // The newly created file was opened with DELETE access. Marking its exact
    // handle avoids resolving the staging name again during failure cleanup.
    unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
}

fn validate_directory(file: &File) -> Result<()> {
    if attributes(file)?.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(denied());
    }
    // Case-sensitive NTFS directories introduce a second namespace semantics;
    // only ordinary case-insensitive directories are currently supported.
    #[repr(C)]
    #[derive(Default)]
    struct CaseInfo {
        flags: u32,
    }
    if information::<CaseInfo>(file, FileCaseSensitiveInfo)?.flags != 0 {
        return Err(WorkerError::new(
            "filesystem_unsupported",
            "Case-sensitive NTFS directories are not supported",
        ));
    }
    Ok(())
}

pub(super) fn identity(file: &File) -> Result<String> {
    let id: FILE_ID_INFO = information(file, FileIdInfo)?;
    // File timestamps are mutable; including them would let a writer change
    // the lease key of a still-live object. NTFS FileId includes its sequence.
    Ok(format!(
        "win-v1:{:016x}:{}",
        id.VolumeSerialNumber,
        id.FileId
            .Identifier
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

pub(super) fn descriptor_path(file: &File) -> Result<PathBuf> {
    let mut value = vec![0u16; 32768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            value.as_mut_ptr(),
            value.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    if length == 0 || length as usize >= value.len() {
        return Err(denied());
    }
    value.truncate(length as usize);
    Ok(PathBuf::from(OsString::from_wide(&value)))
}

fn open_child(parent: &File, name: &str, directory: Option<bool>, create: bool) -> Result<File> {
    let disposition = if create {
        if directory == Some(true) { 3 } else { 2 }
    } else {
        1
    };
    open_child_with_disposition(parent, name, directory, disposition)
}

fn open_child_with_disposition(
    parent: &File,
    name: &str,
    directory: Option<bool>,
    disposition: u32,
) -> Result<File> {
    let create = disposition != 1;
    validate_component(name)?;
    let mut buffer: Vec<u16> = name.encode_utf16().collect();
    let mut name = UNICODE_STRING {
        Length: (buffer.len() * 2) as u16,
        MaximumLength: (buffer.len() * 2) as u16,
        Buffer: buffer.as_mut_ptr(),
    };
    let attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: parent.as_raw_handle(),
        object_name: &mut name,
        // OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE, never OBJ_INHERIT.
        attributes: 0x40 | 0x1000,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut status: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    let mut handle = std::ptr::null_mut();
    let access = if directory == Some(true) {
        FILE_TRAVERSE | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE
    } else {
        FILE_GENERIC_READ
    } | if create && directory != Some(true) {
        FILE_GENERIC_WRITE | DELETE
    } else {
        0
    };
    // FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT.
    let options = 0x00200000
        | 0x20
        | match directory {
            Some(true) => 1,
            Some(false) => 0x40,
            None => 0,
        };
    let code = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            disposition,
            options,
            std::ptr::null(),
            0,
        )
    };
    if code < 0 {
        if matches!(code as u32, 0xc0000034 | 0xc000003a | 0xc000000f) {
            return Err(WorkerError::new(
                "not_found",
                "Workspace file or directory does not exist",
            ));
        }
        return Err(WorkerError::new(
            "path_denied",
            format!(
                "Native workspace open failed (NTSTATUS {:#010x})",
                code as u32
            ),
        ));
    }
    let file = unsafe { File::from_raw_handle(handle) };
    let validation = (|| {
        let attr = attributes_of(&file)?;
        if attr & FILE_ATTRIBUTE_DIRECTORY != 0 {
            validate_directory(&file)?;
        }
        let parent_id: FILE_ID_INFO = information(parent, FileIdInfo)?;
        let child_id: FILE_ID_INFO = information(&file, FileIdInfo)?;
        if parent_id.VolumeSerialNumber != child_id.VolumeSerialNumber {
            return Err(denied());
        }
        Ok(())
    })();
    if validation.is_err() && create && directory != Some(true) {
        delete_on_close(&file);
    }
    validation?;
    Ok(file)
}

fn attributes_of(file: &File) -> Result<u32> {
    Ok(attributes(file)?.FileAttributes)
}

/// Returns all opened ancestors so their no-delete-sharing locks outlive the
/// workspace. No pathname is reopened after a string-only authorization check.
pub(super) fn open_root(path: &Path) -> Result<(File, PathBuf, Vec<File>)> {
    use std::path::{Component, Prefix};
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut components = absolute.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => return Err(denied()),
        },
        _ => return Err(denied()),
    };
    if components.next() != Some(Component::RootDir) {
        return Err(denied());
    }
    let root_path = wide(&format!("{}:\\", drive as char));
    // DRIVE_FIXED. UNC, network mappings, removable/unknown filesystems fail closed.
    if unsafe { GetDriveTypeW(root_path.as_ptr()) } != 3 {
        return Err(WorkerError::new(
            "filesystem_unsupported",
            "Native Windows workspaces require a local fixed NTFS volume",
        ));
    }
    let handle = unsafe {
        CreateFileW(
            root_path.as_ptr(),
            FILE_TRAVERSE | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(denied());
    }
    let mut current = unsafe { File::from_raw_handle(handle) };
    // A drive letter can be a SUBST/DefineDosDevice alias to a directory. Its
    // apparent root would omit real parents from lease ancestry. Check the
    // opened object itself; only a true volume root has this native file name.
    let volume_root: FILE_NAME_INFO = information(&current, FileNameInfo).map_err(|_| {
        WorkerError::new(
            "filesystem_unsupported",
            "Drive aliases to subdirectories are not supported",
        )
    })?;
    if volume_root.FileNameLength != 2 || volume_root.FileName[0] != b'\\' as u16 {
        return Err(WorkerError::new(
            "filesystem_unsupported",
            "Drive aliases to subdirectories are not supported",
        ));
    }
    let mut filesystem = [0u16; 32];
    if unsafe {
        GetVolumeInformationByHandleW(
            current.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    } == 0
    {
        return Err(denied());
    }
    let length = filesystem.iter().position(|c| *c == 0).ok_or_else(denied)?;
    if String::from_utf16_lossy(&filesystem[..length]) != "NTFS" {
        return Err(WorkerError::new(
            "filesystem_unsupported",
            "Native Windows workspaces require NTFS",
        ));
    }
    validate_directory(&current)?;
    let mut ancestors = Vec::new();
    for component in components {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                let next = open_child(
                    &current,
                    name.to_str().ok_or_else(denied)?,
                    Some(true),
                    false,
                )?;
                ancestors.push(current);
                current = next;
            }
            _ => return Err(denied()),
        }
    }
    let normalized = descriptor_path(&current)?;
    Ok((current, normalized, ancestors))
}

/// Provisioning must bind ACL changes to the directory that this operation
/// created, rather than reopen its pathname after a separate mkdir operation.
pub(super) async fn create_native_root(path: &Path) -> Result<Workspace> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(denied)?;
        validate_component(name)?;
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let (parent, _, mut ancestors) = open_root(parent)?;
        // FILE_CREATE, not FILE_OPEN_IF: an existing name is never provisioned.
        // The returned handle and every parent deny delete sharing throughout
        // verification and subsequent ACL changes by the trusted caller.
        let root = open_child_with_disposition(&parent, name, Some(true), 2)?;
        let path = descriptor_path(&root)?;
        let identity = identity(&root)?;
        ancestors.push(parent);
        ancestors.reverse();
        let mut ancestor_ids = vec![identity.clone()];
        for ancestor in &ancestors {
            ancestor_ids.push(self::identity(ancestor)?);
        }
        Ok(Workspace {
            root: Arc::new(root),
            _ancestors: Arc::new(ancestors),
            ancestor_ids: Arc::new(ancestor_ids),
            persistent_identity: true,
            execution_identity: identity.clone(),
            path,
            identity,
        })
    })
    .await
    .map_err(|_| WorkerError::new("io_error", "Workspace provisioning worker failed"))?
}

struct Opened {
    file: File,
    _ancestors: Vec<File>,
}
impl Opened {
    fn open(root: &File, path: &str, directory: Option<bool>) -> Result<Self> {
        relative_path(path)?;
        let mut current = root.try_clone()?;
        let mut ancestors = Vec::new();
        let components: Vec<_> = Path::new(path)
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        for (index, component) in components.iter().enumerate() {
            let next = open_child(
                &current,
                component.to_str().ok_or_else(denied)?,
                if index + 1 < components.len() {
                    Some(true)
                } else {
                    directory
                },
                false,
            )?;
            ancestors.push(current);
            current = next;
        }
        if directory == Some(true) {
            validate_directory(&current)?;
        }
        Ok(Self {
            file: current,
            _ancestors: ancestors,
        })
    }

    fn entries(&self, max: usize) -> Result<Vec<(String, bool, bool)>> {
        validate_directory(&self.file)?;
        // ReOpenFile rejects directory handles. All ancestors are already held
        // without delete sharing, so this handle-derived path cannot be swapped
        // during this open. Confirm the returned object identity as well. The
        // separate handle gives each enumeration an independent kernel cursor.
        let path: Vec<u16> = descriptor_path(&self.file)?
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(native_error("Directory enumeration reopen"));
        }
        let directory = unsafe { File::from_raw_handle(handle) };
        if identity(&directory)? != identity(&self.file)? {
            return Err(WorkerError::new(
                "workspace_conflict",
                "Directory identity changed during enumeration open",
            ));
        }
        validate_directory(&directory)?;
        let mut result = Vec::new();
        let mut restart = true;
        loop {
            // u64 storage provides the required alignment for directory records.
            let mut storage = vec![0u64; 8192];
            let length = storage.len() * size_of::<u64>();
            let pointer = storage.as_mut_ptr().cast::<u8>();
            let class = if restart {
                FileFullDirectoryRestartInfo
            } else {
                FileFullDirectoryInfo
            };
            if unsafe {
                GetFileInformationByHandleEx(
                    directory.as_raw_handle(),
                    class,
                    pointer.cast(),
                    length as u32,
                )
            } == 0
            {
                if std::io::Error::last_os_error().raw_os_error()
                    == Some(ERROR_NO_MORE_FILES as i32)
                {
                    break;
                }
                return Err(native_error("Directory enumeration query"));
            }
            restart = false;
            let mut offset = 0usize;
            loop {
                if offset + size_of::<FILE_FULL_DIR_INFO>() > length {
                    return Err(denied());
                }
                let entry = unsafe {
                    std::ptr::read_unaligned(pointer.add(offset).cast::<FILE_FULL_DIR_INFO>())
                };
                let start = offset + offset_of!(FILE_FULL_DIR_INFO, FileName);
                let end = start
                    .checked_add(entry.FileNameLength as usize)
                    .ok_or_else(denied)?;
                if end > length || entry.FileNameLength % 2 != 0 {
                    return Err(denied());
                }
                let units = unsafe {
                    std::slice::from_raw_parts(
                        pointer.add(start).cast::<u16>(),
                        entry.FileNameLength as usize / 2,
                    )
                };
                let name = String::from_utf16(units).map_err(|_| denied())?;
                if !matches!(name.as_str(), "." | "..") {
                    let safe = validate_component(&name).is_ok()
                        && entry.FileAttributes
                            & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DEVICE)
                            == 0;
                    let is_dir = entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                    result.push((name, safe && is_dir, safe && !is_dir));
                    if result.len() > max {
                        return Err(WorkerError::new(
                            "listing_limit",
                            "Directory scan limit exceeded",
                        ));
                    }
                }
                if entry.NextEntryOffset == 0 {
                    break;
                }
                if (entry.NextEntryOffset as usize) < size_of::<FILE_FULL_DIR_INFO>()
                    || entry.NextEntryOffset % 8 != 0
                {
                    return Err(denied());
                }
                offset = offset
                    .checked_add(entry.NextEntryOffset as usize)
                    .ok_or_else(denied)?;
            }
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }
}

pub(super) fn read(root: &File, path: &str, max: usize) -> Result<Vec<u8>> {
    let opened = Opened::open(root, path, Some(false))?;
    let metadata = opened.file.metadata()?;
    let info: FILE_STANDARD_INFO = information(&opened.file, FileStandardInfo)?;
    if !metadata.is_file() || metadata.len() > max as u64 || info.NumberOfLinks != 1 {
        return Err(WorkerError::new(
            "file_limit",
            "Expected a bounded regular file with one link",
        ));
    }
    let mut content = Vec::new();
    (&opened.file)
        .take(max as u64 + 1)
        .read_to_end(&mut content)?;
    if content.len() > max {
        return Err(WorkerError::new("file_limit", "File grew beyond limit"));
    }
    Ok(content)
}

pub(super) fn create_directory(root: &File, path: &str) -> Result<()> {
    relative_path(path)?;
    let path = Path::new(path);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(denied)?;
    let directory = Opened::open(root, parent.to_str().ok_or_else(denied)?, Some(true))?;
    let _created = open_child(&directory.file, name, Some(true), true)?;
    Ok(())
}

pub(super) fn list(root: &File, path: &str, max: usize) -> Result<Vec<String>> {
    let directory = Opened::open(root, path, Some(true))?;
    let names: Vec<_> = directory
        .entries(20_000)?
        .into_iter()
        .filter(|(name, directory, file)| !name.starts_with('.') && (*directory || *file))
        .map(|(name, _, _)| name)
        .collect();
    if names.len() > max {
        return Err(WorkerError::new(
            "listing_limit",
            "Directory has too many entries",
        ));
    }
    Ok(names)
}

pub(super) fn discover(
    root: &File,
    path: &str,
    recursive: bool,
    hidden: bool,
) -> Result<(Vec<String>, bool)> {
    let first = Opened::open(root, path, None)?;
    if first.file.metadata()?.is_file() {
        return Ok((vec![path.to_owned()], false));
    }
    let mut pending = vec![(path.to_owned(), 0)];
    let mut files = Vec::new();
    let mut scanned = 0;
    let mut incomplete = false;
    while let Some((directory, depth)) = pending.pop() {
        let opened = Opened::open(root, &directory, Some(true))?;
        let entries = match opened.entries(20_000 - scanned) {
            Ok(entries) => entries,
            Err(error) if error.code == "listing_limit" => {
                incomplete = true;
                break;
            }
            Err(error) => return Err(error),
        };
        scanned += entries.len();
        for (name, is_dir, is_file) in entries {
            if matches!(
                name.as_str(),
                ".git" | ".hg" | ".svn" | "node_modules" | "target" | "__pycache__"
            ) || (!hidden && name.starts_with('.'))
            {
                continue;
            }
            let child = if directory == "." {
                name
            } else {
                format!("{directory}/{name}")
            };
            if is_file {
                files.push(child);
            } else if recursive && is_dir {
                if depth < 32 {
                    pending.push((child, depth + 1));
                } else {
                    incomplete = true;
                }
            }
        }
    }
    files.sort();
    Ok((files, incomplete))
}

pub(super) fn write(root: &File, path: &str, bytes: &[u8], expected: Option<&str>) -> Result<()> {
    relative_path(path)?;
    let path = Path::new(path);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(denied)?;
    validate_component(name)?;
    let directory = Opened::open(root, parent.to_str().ok_or_else(denied)?, Some(true))?;
    match (expected, read(&directory.file, name, 1_048_576)) {
        (Some(expected), Ok(content)) if hash(&content) == expected => {}
        (None, Err(error)) if error.code == "not_found" => {}
        _ => {
            return Err(WorkerError::new(
                "write_conflict",
                "Target exists, changed, or is inaccessible",
            ));
        }
    }
    struct Temporary {
        file: File,
        published: bool,
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            if !self.published {
                delete_on_close(&self.file);
            }
        }
    }
    let mut temporary = Temporary {
        file: open_child(
            &directory.file,
            &format!(".werk-{}.tmp", uuid::Uuid::new_v4()),
            Some(false),
            true,
        )?,
        published: false,
    };
    temporary.file.write_all(bytes)?;
    temporary.file.sync_all()?;
    let encoded: Vec<u16> = name.encode_utf16().collect();
    let size = size_of::<FILE_RENAME_INFO>() + encoded.len() * 2;
    let mut storage = vec![0u64; size.div_ceil(8)];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = expected.is_some();
        (*info).RootDirectory = directory.file.as_raw_handle();
        (*info).FileNameLength = (encoded.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            encoded.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            encoded.len(),
        );
        let mut status: IO_STATUS_BLOCK = std::mem::zeroed();
        // FileRenameInformation (10), with the pinned target RootDirectory.
        // Win32 SetFileInformationByHandle rejects this relative-root form.
        let code = NtSetInformationFile(
            temporary.file.as_raw_handle(),
            &mut status,
            info.cast(),
            size as u32,
            10,
        );
        if code < 0 {
            return Err(WorkerError::new(
                "write_conflict",
                format!(
                    "Atomic file publication failed (NTSTATUS {:#010x})",
                    code as u32
                ),
            ));
        }
    }
    temporary.published = true;
    Ok(())
}
