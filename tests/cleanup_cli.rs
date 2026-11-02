use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
    time::{Duration, SystemTime},
};

struct Fixture {
    root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
        };
        for name in ["tmp", "workspace", "home", "state", "cache", "cargo"] {
            fs::create_dir(fixture.root.path().join(name)).unwrap();
        }
        fixture
    }
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.root.path().join(name)
    }
    fn write(&self, name: &str, text: &str) {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ww"));
        command.current_dir(self.path("workspace"));
        for (variable, name) in [
            ("HOME", "home"),
            ("USERPROFILE", "home"),
            ("XDG_STATE_HOME", "state"),
            ("LOCALAPPDATA", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("CARGO_HOME", "cargo"),
            ("TMPDIR", "tmp"),
            ("TMP", "tmp"),
            ("TEMP", "tmp"),
        ] {
            command.env(variable, self.path(name));
        }
        command
    }
    fn output(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn old(path: &Path) {
    let file = if path.is_dir() {
        #[cfg(unix)]
        {
            fs::File::open(path).unwrap()
        }
        #[cfg(not(unix))]
        {
            return;
        }
    } else {
        fs::OpenOptions::new().write(true).open(path).unwrap()
    };
    file.set_modified(SystemTime::now() - Duration::from_secs(48 * 3600))
        .unwrap();
}

#[test]
fn tmp_selection_dry_run_age_and_unrelated_files() {
    let f = Fixture::new();
    f.write("tmp/ww-old.log", "old");
    f.write("tmp/ww-recent.log", "recent");
    f.write("tmp/werk-worker-directory/current.log", "recent descendant");
    f.write("tmp/unrelated.log", "keep");
    f.write("tmp/werk1112-test.log", "keep");
    old(&f.path("tmp/ww-old.log"));
    old(&f.path("tmp/werk-worker-directory"));
    let inventory = f.json(&["tmp", "list", "--json"]);
    assert_eq!(inventory["entries"].as_array().unwrap().len(), 3);
    let entries = inventory["entries"].as_array().unwrap();
    assert_eq!(entries.iter().filter(|e| e["blocked"].is_null()).count(), 1);
    let selected = entries.iter().find(|e| e["blocked"].is_null()).unwrap()["id"]
        .as_str()
        .unwrap();
    let preview = f.json(&["tmp", "purge", "--all", "--dry-run", "--json"]);
    assert_eq!(preview["removed"].as_array().unwrap().len(), 0);
    assert!(f.path("tmp/ww-old.log").exists());
    assert!(!f.output(&["tmp", "purge"]).status.success());
    assert!(
        !f.output(&["tmp", "purge", selected, "--all"])
            .status
            .success()
    );
    assert!(
        !f.output(&["tmp", "purge", "../unrelated.log"])
            .status
            .success()
    );
    f.json(&["tmp", "purge", selected, "--json"]);
    assert!(!f.path("tmp/ww-old.log").exists());
    assert!(f.path("tmp/ww-recent.log").exists());
    assert!(f.path("tmp/werk-worker-directory/current.log").exists());
    let removed = f.json(&[
        "temp",
        "purge",
        "--all",
        "--older-than-hours",
        "0",
        "--json",
    ]);
    assert_eq!(removed["removed"].as_array().unwrap().len(), 2);
    assert!(f.path("tmp/unrelated.log").exists());
    assert!(f.path("tmp/werk1112-test.log").exists());
    assert_eq!(fs::read_dir(f.path("state")).unwrap().count(), 0);
}

#[test]
fn cache_scope_preserves_source_sessions_models_credentials_and_installed_programs() {
    let f = Fixture::new();
    f.write(
        "workspace/Cargo.toml",
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    );
    f.write("workspace/target/debug/build.o", "compiled");
    f.write(
        "workspace/examples/sdk/Cargo.toml",
        "[package]\nname='sdk'\nversion='0.1.0'\n",
    );
    f.write("workspace/examples/sdk/target/build.o", "compiled");
    f.write("workspace/unrelated/target/keep.txt", "not a Rust project");
    f.write("workspace/src/main.rs", "fn main() {}");
    f.write("state/werk-worker/backups/old/ww", "backup");
    f.write("state/werk-worker/sessions.sqlite", "history");
    f.write("cache/pip/download", "cache");
    f.write("cache/huggingface/hub/model", "model");
    f.write("cache/huggingface/token", "credential fixture");
    f.write("cargo/bin/ww", "installed");
    let inventory = f.json(&["cache", "list", "--json"]);
    assert_eq!(inventory["entries"].as_array().unwrap().len(), 3);
    assert!(
        inventory["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["kind"] != "shared")
    );
    f.json(&[
        "cache",
        "purge",
        "--all",
        "--kind",
        "build",
        "--dry-run",
        "--json",
    ]);
    assert!(f.path("workspace/target/debug/build.o").exists());
    f.json(&["cache", "purge", "--all", "--kind", "build", "--json"]);
    assert!(!f.path("workspace/target").exists());
    assert!(!f.path("workspace/examples/sdk/target").exists());
    assert!(f.path("state/werk-worker/backups/old/ww").exists());
    f.json(&["cache", "purge", "--all", "--kind", "backup", "--json"]);
    assert!(!f.path("state/werk-worker/backups/old").exists());
    assert!(f.path("cache/pip/download").exists());
    f.json(&["cache", "purge", "--all", "--kind", "shared", "--json"]);
    assert!(!f.path("cache/pip").exists());
    for name in [
        "workspace/src/main.rs",
        "workspace/unrelated/target/keep.txt",
        "state/werk-worker/sessions.sqlite",
        "cache/huggingface/hub/model",
        "cache/huggingface/token",
        "cargo/bin/ww",
    ] {
        assert!(f.path(name).exists(), "removed {name}");
    }
}

#[cfg(unix)]
#[test]
fn symlinks_never_expand_cleanup_scope_and_hard_links_are_counted_once() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    f.write("outside/keep", "outside");
    symlink(f.path("outside"), f.path("tmp/ww-outside")).unwrap();
    f.write("tmp/ww-owned/data", &"x".repeat(8192));
    fs::hard_link(f.path("tmp/ww-owned/data"), f.path("tmp/ww-owned/hardlink")).unwrap();
    symlink(f.path("outside"), f.path("tmp/ww-owned/link")).unwrap();
    let inventory = f.json(&["tmp", "list", "--older-than-hours", "0", "--json"]);
    let owned = inventory["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["blocked"].is_null())
        .unwrap();
    assert_eq!(owned["files"], 2);
    use std::os::unix::fs::MetadataExt;
    let expected = fs::symlink_metadata(f.path("tmp/ww-owned"))
        .unwrap()
        .blocks()
        * 512
        + fs::metadata(f.path("tmp/ww-owned/data")).unwrap().blocks() * 512
        + fs::symlink_metadata(f.path("tmp/ww-owned/link"))
            .unwrap()
            .blocks()
            * 512;
    assert_eq!(owned["allocated_bytes"], expected);
    f.json(&["tmp", "purge", "--all", "--older-than-hours", "0", "--json"]);
    assert!(!f.path("tmp/ww-owned").exists());
    assert!(f.path("tmp/ww-outside").is_symlink());
    assert!(f.path("outside/keep").exists());
    fs::create_dir_all(f.path("state/werk-worker")).unwrap();
    symlink(f.path("outside"), f.path("state/werk-worker/backups")).unwrap();
    let report = f.json(&["cache", "purge", "--all", "--kind", "backup", "--json"]);
    assert!(report["entries"][0]["blocked"].is_string());
    assert!(f.path("outside/keep").exists());
}

#[test]
fn active_cargo_locks_block_build_purging() {
    use fs2::FileExt;
    let f = Fixture::new();
    f.write(
        "workspace/Cargo.toml",
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    );
    f.write("workspace/target/debug/.cargo-lock", "");
    f.write("workspace/target/debug/build.o", &"x".repeat(8192));
    let lock = fs::File::open(f.path("workspace/target/debug/.cargo-lock")).unwrap();
    lock.lock_exclusive().unwrap();
    let report = f.json(&["cache", "purge", "--all", "--kind", "build", "--json"]);
    assert!(
        report["entries"][0]["blocked"]
            .as_str()
            .unwrap()
            .contains("Cargo")
    );
    assert_eq!(report["entries"][0]["files"], 2);
    assert_eq!(report["entries"][0]["size_complete"], true);
    assert!(f.path("workspace/target").exists());
    drop(lock);
    f.json(&["cache", "purge", "--all", "--kind", "build", "--json"]);
    assert!(!f.path("workspace/target").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn active_process_working_directory_is_not_purged() {
    let f = Fixture::new();
    fs::create_dir(f.path("tmp/ww-active")).unwrap();
    let mut child = Command::new("sleep")
        .arg("30")
        .current_dir(f.path("tmp/ww-active"))
        .spawn()
        .unwrap();
    let output = f.output(&["tmp", "purge", "--all", "--older-than-hours", "0", "--json"]);
    let _ = child.kill();
    let _ = child.wait();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["entries"][0]["blocked"]
            .as_str()
            .unwrap()
            .contains("process")
    );
    assert!(f.path("tmp/ww-active").exists());
}
