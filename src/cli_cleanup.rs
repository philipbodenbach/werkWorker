//! Host CLI maintenance. Never exposed as an agent tool.
use clap::{Args, Subcommand, ValueEnum};
use fs2::FileExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, Metadata},
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use werk_worker::{Result, WorkerError};

#[derive(Clone, Subcommand)]
pub enum TmpCommand {
    /// List owned WerkWorker entries in the system temporary directory.
    List {
        #[arg(long)]
        json: bool,
        /// Entries modified more recently remain protected (including descendants).
        #[arg(long, default_value_t = 24)]
        older_than_hours: u64,
    },
    /// Remove one listed ID or all eligible WerkWorker temporary entries.
    Purge {
        #[command(flatten)]
        selection: Selection,
        #[arg(long, default_value_t = 24)]
        older_than_hours: u64,
    },
    /// Print the system temporary directory; only WerkWorker entries are managed.
    Path,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Build,
    Backup,
    Shared,
    #[value(skip)]
    Temporary,
}

#[derive(Clone, Args)]
pub struct Scope {
    /// Restrict entries to build, backup or shared storage.
    #[arg(long, value_enum)]
    kind: Option<Kind>,
    /// Include shared package/GPU caches used by other projects (no model weights).
    #[arg(long)]
    shared: bool,
}

#[derive(Clone, Args)]
#[group(skip)]
pub struct Selection {
    /// Exact ID printed by list; paths and wildcards are not accepted.
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    id: Option<String>,
    /// Select all eligible entries in the requested scope.
    #[arg(long)]
    all: bool,
    /// Show what would be removed without deleting anything.
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Subcommand)]
pub enum CacheCommand {
    /// List target directories in --workspace and saved program backups.
    List {
        #[command(flatten)]
        scope: Scope,
        #[arg(long)]
        json: bool,
    },
    /// Remove one listed ID or all eligible entries in the selected scope.
    Purge {
        #[command(flatten)]
        selection: Selection,
        #[command(flatten)]
        scope: Scope,
    },
}

#[derive(Clone)]
struct Candidate {
    root: PathBuf,
    path: PathBuf,
    kind: Kind,
}

#[derive(Serialize)]
struct Entry {
    id: String,
    kind: Kind,
    path: PathBuf,
    files: u64,
    allocated_bytes: u64,
    size_complete: bool,
    blocked: Option<String>,
}

#[derive(Default, Serialize)]
struct Report {
    dry_run: bool,
    entries: Vec<Entry>,
    eligible_bytes: u64,
    removed: Vec<PathBuf>,
    errors: Vec<String>,
}

pub fn temporary(command: &TmpCommand) -> Result<()> {
    let root = fs::canonicalize(std::env::temp_dir())?;
    if matches!(command, TmpCommand::Path) {
        println!("{}", root.display());
        return Ok(());
    }
    let candidates = fs::read_dir(&root)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            ["ww-", "ww_", "werk-worker-", "werkworker-", "werk_doctor_"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
        })
        .map(|path| Candidate {
            root: root.clone(),
            path,
            kind: Kind::Temporary,
        })
        .collect();
    match command {
        TmpCommand::List {
            json,
            older_than_hours,
        } => run(candidates, *older_than_hours, None, *json),
        TmpCommand::Purge {
            selection,
            older_than_hours,
        } => run(
            candidates,
            *older_than_hours,
            Some(selection),
            selection.json,
        ),
        TmpCommand::Path => unreachable!(),
    }
}

pub fn cache(command: &CacheCommand, workspace: &Path) -> Result<()> {
    let (scope, selection, json) = match command {
        CacheCommand::List { scope, json } => (scope, None, *json),
        CacheCommand::Purge { scope, selection } => (scope, Some(selection), selection.json),
    };
    let mut candidates = Vec::new();
    if scope.kind.is_none() || scope.kind == Some(Kind::Build) {
        let workspace = fs::canonicalize(workspace)?;
        if workspace.join("Cargo.toml").is_file() {
            discover_builds(&workspace, &workspace, 0, &mut candidates)?;
        }
    }
    if scope.kind.is_none() || scope.kind == Some(Kind::Backup) {
        let state = state_home()?;
        for suffix in ["werk-worker/backups", "werk-worker/binary-backups"] {
            add_children(&state, suffix, Kind::Backup, &mut candidates)?;
        }
    }
    if scope.shared || scope.kind == Some(Kind::Shared) {
        let home = user_home()?;
        let cache = env_path("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache"));
        let cargo = env_path("CARGO_HOME").unwrap_or_else(|| home.join(".cargo"));
        for (root, suffix) in [
            (&cache, "pip"),
            (&cache, "uv"),
            (&cache, "pipenv"),
            (&cache, "vllm"),
            (&home, ".npm/_cacache"),
            (&home, ".nv/ComputeCache"),
            (&home, ".triton/cache"),
            (&cargo, "registry/cache"),
            (&cargo, "registry/src"),
            (&cargo, "registry/index"),
        ] {
            add(root, suffix, Kind::Shared, &mut candidates)?;
        }
    }
    if let Some(kind) = scope.kind {
        candidates.retain(|candidate| candidate.kind == kind);
    }
    run(candidates, 0, selection, json)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn user_home() -> Result<PathBuf> {
    env_path(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .ok_or_else(|| WorkerError::new("cleanup_home", "User home directory is not configured"))
}

fn state_home() -> Result<PathBuf> {
    if cfg!(windows) {
        env_path("LOCALAPPDATA")
            .ok_or_else(|| WorkerError::new("cleanup_home", "LOCALAPPDATA is not configured"))
    } else {
        match env_path("XDG_STATE_HOME") {
            Some(path) => Ok(path),
            None => Ok(user_home()?.join(".local/state")),
        }
    }
}

fn add(root: &Path, suffix: &str, kind: Kind, out: &mut Vec<Candidate>) -> Result<()> {
    if !root.is_absolute() {
        return Err(WorkerError::new(
            "cleanup_path",
            "Cleanup roots must be absolute paths",
        ));
    }
    if !root.exists() {
        return Ok(());
    }
    let root = fs::canonicalize(root)?;
    let path = root.join(suffix);
    match fs::symlink_metadata(&path) {
        Ok(_) => out.push(Candidate { root, path, kind }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn add_children(root: &Path, suffix: &str, kind: Kind, out: &mut Vec<Candidate>) -> Result<()> {
    let mut parents = Vec::new();
    add(root, suffix, kind, &mut parents)?;
    for parent in parents {
        // Never traverse a redirected backup directory.
        if safe_path(&parent).is_err() {
            out.push(parent);
            continue;
        }
        for child in fs::read_dir(&parent.path)? {
            out.push(Candidate {
                path: child?.path(),
                ..parent.clone()
            });
        }
    }
    Ok(())
}

fn discover_builds(
    root: &Path,
    directory: &Path,
    depth: usize,
    out: &mut Vec<Candidate>,
) -> Result<()> {
    if directory.join("Cargo.toml").is_file() {
        let manifest: toml::Value =
            toml::from_str(&fs::read_to_string(directory.join("Cargo.toml"))?)
                .map_err(|error| WorkerError::new("cleanup_manifest", error.to_string()))?;
        if manifest.get("package").is_some() || manifest.get("workspace").is_some() {
            let suffix = directory.strip_prefix(root).unwrap().join("target");
            add(root, &suffix.to_string_lossy(), Kind::Build, out)?;
        }
    }
    if depth >= 4 {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || matches!(name.as_ref(), "target" | "node_modules" | "vendor") {
            continue;
        }
        if entry.file_type()?.is_dir() {
            discover_builds(root, &entry.path(), depth + 1, out)?;
        }
    }
    Ok(())
}

fn safe_path(candidate: &Candidate) -> io::Result<()> {
    let relative = candidate
        .path
        .strip_prefix(&candidate.root)
        .map_err(|_| io::Error::other("Path is outside its cleanup root"))?;
    if relative.as_os_str().is_empty() {
        return Err(io::Error::other("Cannot delete a cleanup root"));
    }
    let mut path = candidate.root.clone();
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(io::Error::other("Invalid cleanup path"));
        }
        path.push(component);
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::other("Symbolic links are protected"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::other("Entry belongs to another user"));
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct Usage {
    files: u64,
    bytes: u64,
    newest: Option<SystemTime>,
    seen: HashSet<(u64, u64)>,
    locks: Vec<File>,
    blocked: Option<String>,
}

fn measure(path: &Path, usage: &mut Usage) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    usage.newest = usage.newest.max(Some(metadata.modified()?));
    if metadata.is_file() {
        usage.files += 1;
    }
    usage.bytes = usage
        .bytes
        .saturating_add(allocated(&metadata, &mut usage.seen));
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file()
        && path
            .file_name()
            .is_some_and(|name| name == ".cargo-lock" || name == ".package-cache")
    {
        let file = File::open(path)?;
        if file.try_lock_exclusive().is_ok() {
            usage.locks.push(file);
        } else {
            // Listing sizes remains useful during a build; never purge the tree.
            usage.blocked = Some("Cargo is using this directory".into());
        }
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            measure(&entry?.path(), usage)?;
        }
    } else if !metadata.is_file() {
        return Err(io::Error::other("Special files are protected"));
    }
    Ok(())
}

fn allocated(metadata: &Metadata, seen: &mut HashSet<(u64, u64)>) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if seen.insert((metadata.dev(), metadata.ino())) {
            metadata.blocks().saturating_mul(512)
        } else {
            0
        }
    }
    #[cfg(not(unix))]
    {
        let _ = seen;
        metadata.len()
    }
}

/// Best-effort Linux process snapshot: never read process environments or file contents.
#[cfg(target_os = "linux")]
fn active_paths() -> Vec<(PathBuf, u32)> {
    let mut paths = Vec::new();
    if let Ok(processes) = fs::read_dir("/proc") {
        use std::os::unix::fs::MetadataExt;
        for process in processes.flatten() {
            let Ok(pid) = process.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if pid == std::process::id()
                || process
                    .metadata()
                    .map_or(true, |m| m.uid() != unsafe { libc::geteuid() })
            {
                continue;
            }
            for name in ["exe", "cwd"] {
                if let Ok(path) = fs::read_link(process.path().join(name)) {
                    paths.push((path, pid));
                }
            }
            if let Ok(fds) = fs::read_dir(process.path().join("fd")) {
                for fd in fds.flatten() {
                    if let Ok(path) = fs::read_link(fd.path()) {
                        paths.push((path, pid));
                    }
                }
            }
        }
    }
    paths
}

#[cfg(not(target_os = "linux"))]
fn active_paths() -> Vec<(PathBuf, u32)> {
    Vec::new()
}

fn inspect(candidate: &Candidate, hours: u64, active: &[(PathBuf, u32)]) -> (Entry, Usage) {
    let id = format!(
        "{:x}",
        Sha256::digest(format!("{:?}", candidate.path).as_bytes())
    )[..16]
        .to_owned();
    let mut usage = Usage::default();
    let measured = safe_path(candidate).and_then(|()| measure(&candidate.path, &mut usage));
    let size_complete = measured.is_ok();
    let mut blocked = measured
        .err()
        .map(|e| e.to_string())
        .or_else(|| usage.blocked.clone());
    if blocked.is_none()
        && hours > 0
        && usage.newest.is_some_and(|time| {
            SystemTime::now().duration_since(time).unwrap_or_default()
                < Duration::from_secs(hours.saturating_mul(3600))
        })
    {
        blocked = Some(format!("Modified within the last {hours} hours"));
    }
    if blocked.is_none()
        && let Some((_, pid)) = active
            .iter()
            .find(|(path, _)| path.starts_with(&candidate.path))
    {
        blocked = Some(format!("In use by process {pid}"));
    }
    (
        Entry {
            id,
            kind: candidate.kind,
            path: candidate.path.clone(),
            files: usage.files,
            allocated_bytes: usage.bytes,
            size_complete,
            blocked,
        },
        usage,
    )
}

fn run(
    mut candidates: Vec<Candidate>,
    hours: u64,
    selection: Option<&Selection>,
    json: bool,
) -> Result<()> {
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    candidates.dedup_by(|a, b| a.path == b.path);
    let active = active_paths();
    let mut report = Report {
        dry_run: selection.is_some_and(|s| s.dry_run),
        ..Default::default()
    };
    let mut selected = false;
    for candidate in candidates {
        let (mut entry, _locks) = inspect(&candidate, hours, &active);
        if let Some(selection) = selection {
            if !selection.all && selection.id.as_deref() != Some(&entry.id) {
                continue;
            }
            selected = true;
            if entry.blocked.is_none() && !selection.dry_run {
                // Recheck roots immediately before removal. remove_dir_all does not follow child links.
                let removed = safe_path(&candidate).and_then(|()| {
                    if fs::symlink_metadata(&candidate.path)?.is_dir() {
                        fs::remove_dir_all(&candidate.path)
                    } else {
                        fs::remove_file(&candidate.path)
                    }
                });
                match removed {
                    Ok(()) => report.removed.push(candidate.path.clone()),
                    Err(error) => {
                        let message = format!("{}: {error}", candidate.path.display());
                        entry.blocked = Some(message.clone());
                        report.errors.push(message);
                    }
                }
            }
        }
        if entry.blocked.is_none() {
            report.eligible_bytes = report.eligible_bytes.saturating_add(entry.allocated_bytes);
        }
        report.entries.push(entry);
    }
    if selection.is_some_and(|s| s.id.is_some()) && !selected {
        return Err(WorkerError::new(
            "cleanup_not_found",
            "Unknown ID in this scope; run list with the same options first",
        ));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for entry in &report.entries {
            let kind = match entry.kind {
                Kind::Temporary => "tmp",
                Kind::Build => "build",
                Kind::Backup => "backup",
                Kind::Shared => "shared",
            };
            println!(
                "{}  {:6}  {:>9.1} MiB  {:>7} files  {}\n  {}",
                entry.id,
                kind,
                entry.allocated_bytes as f64 / 1048576.0,
                entry.files,
                entry.path.display(),
                entry.blocked.as_deref().unwrap_or(if selection.is_none() {
                    "eligible"
                } else if report.dry_run {
                    "would remove"
                } else {
                    "removed"
                })
            );
            if !entry.size_complete {
                println!("  Size is incomplete: this entry could not be fully inspected.");
            }
        }
        println!(
            "{} entries; {:.1} MiB eligible; {} removed{}",
            report.entries.len(),
            report.eligible_bytes as f64 / 1048576.0,
            report.removed.len(),
            if report.dry_run { " (dry run)" } else { "" }
        );
        if selection.is_none() {
            println!(
                "Purge requires an ID or --all. Use --dry-run to preview. Sizes count allocated storage; hard links can reduce reclaimed space."
            );
        }
    }
    if !report.errors.is_empty()
        || (selection.is_some_and(|s| s.id.is_some())
            && report.entries.iter().any(|e| e.blocked.is_some()))
    {
        return Err(WorkerError::new(
            "cleanup_blocked",
            "Selected entries could not be removed; see the report",
        ));
    }
    Ok(())
}
