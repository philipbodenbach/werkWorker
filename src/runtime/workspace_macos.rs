//! Descriptor-relative local APFS/HFS workspace access.
//!
//! Held descriptors authorize objects, not future objects at the same names.
//! Edge checks detect namespace changes; they are not transactions against an
//! unrelated process renaming an already opened directory during an operation.
use super::*;
use std::ffi::{CStr, CString, OsStr};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

fn denied() -> WorkerError {
    WorkerError::new(
        "path_denied",
        "Unsafe or inaccessible native workspace path",
    )
}

fn open_error() -> WorkerError {
    if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
        WorkerError::new("not_found", "Workspace file or directory does not exist")
    } else {
        denied()
    }
}

fn open_at(parent: &File, name: &CStr, flags: i32, mode: libc::mode_t) -> Result<File> {
    // SAFETY: parent is live and name is NUL-terminated; flags never follow links.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(open_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) fn descriptor_path(file: &File) -> Result<PathBuf> {
    let mut bytes = [0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes into the provided buffer.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) } != 0 {
        return Err(denied());
    }
    let end = bytes.iter().position(|b| *b == 0).ok_or_else(denied)?;
    Ok(PathBuf::from(OsStr::from_bytes(&bytes[..end])))
}

pub(super) fn open_root(path: &Path) -> Result<(File, PathBuf)> {
    // Canonicalization selects the host-requested root. Authority comes from
    // the descriptor walk below, not reopening a checked string for each tool.
    let selected = std::fs::canonicalize(path)?;
    let mut options = std::fs::OpenOptions::new();
    use std::os::unix::fs::OpenOptionsExt;
    let mut current = options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open("/")?;
    for component in selected.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                let name = CString::new(name.as_bytes()).map_err(|_| denied())?;
                current = open_at(&current, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            }
            _ => return Err(denied()),
        }
    }
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: statfs is initialized storage and current is a live descriptor.
    if unsafe { libc::fstatfs(current.as_raw_fd(), &mut fs) } != 0 {
        return Err(denied());
    }
    let kind = unsafe { CStr::from_ptr(fs.f_fstypename.as_ptr()) }.to_bytes();
    if !matches!(kind, b"apfs" | b"hfs") || fs.f_flags & libc::MNT_LOCAL as u32 == 0 {
        return Err(WorkerError::new(
            "filesystem_unsupported",
            "Native macOS workspaces require a local APFS or HFS+ filesystem",
        ));
    }
    let path = descriptor_path(&current)?;
    Ok((current, path))
}

struct Edge {
    parent: File,
    name: CString,
    device: u64,
    inode: u64,
}

pub(super) struct Opened {
    pub(super) file: File,
    edges: Vec<Edge>,
}

impl Opened {
    pub(super) fn open(root: &File, path: &str, directory: bool) -> Result<Self> {
        relative_path(path)?;
        let components: Vec<_> = Path::new(path)
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        let root_device = root.metadata()?.dev();
        let mut current = open_at(root, c".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        let mut edges = Vec::new();
        for (index, name) in components.iter().enumerate() {
            let name = CString::new(name.as_bytes()).map_err(|_| denied())?;
            let needs_directory = directory || index + 1 < components.len();
            let flags = libc::O_RDONLY
                | if needs_directory {
                    libc::O_DIRECTORY
                } else {
                    0
                };
            let next = open_at(&current, &name, flags, 0)?;
            let metadata = next.metadata()?;
            if metadata.dev() != root_device || (!metadata.is_file() && !metadata.is_dir()) {
                return Err(denied());
            }
            edges.push(Edge {
                parent: current,
                name,
                device: metadata.dev(),
                inode: metadata.ino(),
            });
            current = next;
        }
        let opened = Self {
            file: current,
            edges,
        };
        opened.validate()?;
        Ok(opened)
    }

    pub(super) fn validate(&self) -> Result<()> {
        for edge in &self.edges {
            let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: descriptors, CString and output storage are live.
            if unsafe {
                libc::fstatat(
                    edge.parent.as_raw_fd(),
                    edge.name.as_ptr(),
                    &mut metadata,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
                || metadata.st_dev as u64 != edge.device
                || metadata.st_ino != edge.inode
                || metadata.st_mode & libc::S_IFMT == libc::S_IFLNK
            {
                return Err(WorkerError::new(
                    "workspace_conflict",
                    "Workspace parent changed",
                ));
            }
        }
        Ok(())
    }

    fn entries(&self, max: usize) -> Result<Vec<(String, bool, bool)>> {
        use std::os::fd::IntoRawFd;
        let directory = open_at(&self.file, c".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        let fd = directory.into_raw_fd();
        // SAFETY: fd ownership is transferred to fdopendir on success.
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            unsafe { libc::close(fd) };
            return Err(denied());
        }
        struct Directory(*mut libc::DIR);
        impl Drop for Directory {
            fn drop(&mut self) {
                unsafe { libc::closedir(self.0) };
            }
        }
        let stream = Directory(stream);
        let mut result = Vec::new();
        loop {
            unsafe { *libc::__error() = 0 };
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                if std::io::Error::last_os_error().raw_os_error() != Some(0) {
                    return Err(denied());
                }
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            let name = name.to_str().map_err(|_| denied())?.to_owned();
            let c_name = CString::new(name.as_bytes()).map_err(|_| denied())?;
            let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    c_name.as_ptr(),
                    &mut metadata,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(WorkerError::new(
                    "workspace_conflict",
                    "Directory changed during listing",
                ));
            }
            let kind = metadata.st_mode & libc::S_IFMT;
            let same_volume = metadata.st_dev as u64 == self.file.metadata()?.dev();
            result.push((
                name,
                same_volume && kind == libc::S_IFDIR,
                same_volume && kind == libc::S_IFREG,
            ));
            if result.len() > max {
                return Err(WorkerError::new(
                    "listing_limit",
                    "Directory scan limit exceeded",
                ));
            }
        }
        self.validate()?;
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }
}

pub(super) fn read(root: &File, path: &str, max: usize) -> Result<Vec<u8>> {
    let opened = Opened::open(root, path, false)?;
    let metadata = opened.file.metadata()?;
    if !metadata.is_file() || metadata.len() > max as u64 {
        return Err(WorkerError::new("file_limit", "Not a bounded regular file"));
    }
    let mut data = Vec::new();
    (&opened.file).take(max as u64 + 1).read_to_end(&mut data)?;
    opened.validate()?;
    if data.len() > max {
        return Err(WorkerError::new("file_limit", "File grew beyond limit"));
    }
    Ok(data)
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
    let directory = Opened::open(root, parent.to_str().ok_or_else(denied)?, true)?;
    let name_c = CString::new(name).map_err(|_| denied())?;
    directory.validate()?;
    let result = unsafe { libc::mkdirat(directory.file.as_raw_fd(), name_c.as_ptr(), 0o700) };
    if result != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
        return Err(open_error());
    }
    // Existing symlinks/files and cross-volume directory mounts remain denied.
    let _created = Opened::open(&directory.file, name, true)?;
    directory.validate()
}

pub(super) fn list(root: &File, path: &str, max: usize) -> Result<Vec<String>> {
    let directory = Opened::open(root, path, true)?;
    let names: Vec<_> = directory
        .entries(20_000)?
        .into_iter()
        .filter(|(name, dir, file)| !name.starts_with('.') && (*dir || *file))
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
    let first = Opened::open(root, path, false)?;
    if first.file.metadata()?.is_file() {
        return Ok((vec![path.to_owned()], false));
    }
    let mut pending = vec![(path.to_owned(), 0)];
    let mut files = Vec::new();
    let mut scanned = 0;
    let mut incomplete = false;
    while let Some((directory, depth)) = pending.pop() {
        let opened = Opened::open(root, &directory, true)?;
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
    let directory = Opened::open(root, parent.to_str().ok_or_else(denied)?, true)?;
    let existing = read(&directory.file, name, 1_048_576);
    match (expected, existing) {
        (Some(expected), Ok(bytes)) if hash(&bytes) == expected => {}
        (None, Err(error)) if error.code == "not_found" => {}
        _ => {
            return Err(WorkerError::new(
                "write_conflict",
                "Target exists, changed, or is inaccessible",
            ));
        }
    }
    let temp = CString::new(format!(".werk-{}.tmp", uuid::Uuid::new_v4())).unwrap();
    let mut file = open_at(
        &directory.file,
        &temp,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    struct Temporary<'a> {
        directory: &'a File,
        name: CString,
        published: bool,
    }
    impl Drop for Temporary<'_> {
        fn drop(&mut self) {
            if !self.published {
                unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0) };
            }
        }
    }
    let mut temporary = Temporary {
        directory: &directory.file,
        name: temp,
        published: false,
    };
    file.write_all(bytes)?;
    file.sync_all()?;
    directory.validate()?;
    let target = CString::new(name).map_err(|_| denied())?;
    // Atomic publication, not a transaction against arbitrary external writers.
    let flags = if expected.is_none() {
        libc::RENAME_EXCL
    } else {
        0
    };
    if unsafe {
        libc::renameatx_np(
            directory.file.as_raw_fd(),
            temporary.name.as_ptr(),
            directory.file.as_raw_fd(),
            target.as_ptr(),
            flags,
        )
    } != 0
    {
        return Err(WorkerError::new(
            "write_conflict",
            "Atomic file publication failed",
        ));
    }
    temporary.published = true;
    directory.file.sync_all()?;
    directory.validate()?;
    Ok(())
}
