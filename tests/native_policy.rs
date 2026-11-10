//! Native admission and persisted approval checks through the shared engine.
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{path::PathBuf, process::Command, sync::Arc, time::Duration};
use werk_worker::{
    policy::{HostPolicy, ToolPermission},
    profiles::AgentProfile,
    providers::{ScriptedProvider, ScriptedTurn},
    runtime::{NativeSandboxConfig, Sandbox, Workspace},
    sessions::SqliteStore,
    *,
};

fn home_key() -> &'static str {
    if cfg!(windows) { "USERPROFILE" } else { "HOME" }
}

#[tokio::test]
async fn missing_home_fixture() {
    let Some(path) = std::env::var_os("WERK_POLICY_HOME_FIXTURE") else {
        return;
    };
    let workspace = PathBuf::from(path);
    let reject_home = std::env::var_os("WERK_POLICY_HOME_REJECT").is_some();
    assert_eq!(
        PathBuf::from(std::env::var_os(home_key()).unwrap()).exists(),
        reject_home || std::env::var_os("WERK_POLICY_HOME_EXISTS").is_some()
    );
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
        .build()
        .await
        .unwrap();
    let admission = engine
        .create_session(
            AgentProfile::bundled("general-readonly").unwrap(),
            &workspace,
            HostPolicy::default(),
            Value::Null,
        )
        .await;
    if reject_home {
        assert_eq!(admission.err().unwrap().code, "unsafe_workspace");
        engine.close().await.unwrap();
        return;
    }
    let session =
        admission.expect("missing inherited home must not reject another valid directory");
    assert_eq!(
        session.snapshot().await.unwrap().workspace_identity,
        Workspace::open(&workspace).await.unwrap().identity
    );
    engine.close().await.unwrap();
}

#[test]
fn missing_home_does_not_block_session_admission() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "missing_home_fixture", "--nocapture"])
        .env("WERK_POLICY_HOME_FIXTURE", &workspace)
        .env_remove("WERK_POLICY_HOME_REJECT")
        .env_remove("WERK_POLICY_HOME_EXISTS")
        .env(home_key(), root.path().join("removed-home"))
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}

#[test]
fn existing_home_identity_remains_protected() {
    let root = tempfile::tempdir().unwrap();
    let home = if cfg!(windows) {
        PathBuf::from(root.path().to_string_lossy().to_uppercase())
    } else {
        root.path().to_owned()
    };
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "missing_home_fixture", "--nocapture"])
        .env("WERK_POLICY_HOME_FIXTURE", root.path())
        .env("WERK_POLICY_HOME_REJECT", "1")
        .env(home_key(), home)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}

#[test]
fn home_ancestor_is_rejected_while_sibling_workspace_is_allowed() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("private-home");
    let sibling = root.path().join("workspace");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    for (workspace, reject) in [(root.path(), true), (sibling.as_path(), false)] {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "missing_home_fixture", "--nocapture"])
            .env("WERK_POLICY_HOME_FIXTURE", workspace)
            .env("WERK_POLICY_HOME_EXISTS", "1")
            .env_remove("WERK_POLICY_HOME_REJECT")
            .env(home_key(), &home);
        if reject {
            child.env("WERK_POLICY_HOME_REJECT", "1");
        }
        let result = child.output().unwrap();
        assert!(
            result.status.success(),
            "{workspace:?}: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[tokio::test]
async fn artifact_store_directory_is_protected_by_identity() {
    let root = tempfile::tempdir().unwrap();
    let artifacts = root.path().join("Control Artifacts");
    let store = werk_worker::runtime::ArtifactStore::open(&artifacts)
        .await
        .unwrap();
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
        .artifacts(store)
        .build()
        .await
        .unwrap();
    let alias = if cfg!(windows) {
        PathBuf::from(artifacts.to_string_lossy().to_uppercase())
    } else {
        artifacts
    };
    let error = engine
        .create_session(
            AgentProfile::bundled("general-readonly").unwrap(),
            alias,
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "unsafe_workspace");
    engine.close().await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn case_alias_does_not_expose_the_session_database() {
    let root = tempfile::tempdir().unwrap();
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
        .store(Arc::new(
            SqliteStore::open(root.path().join("session.sqlite"))
                .await
                .unwrap(),
        ))
        .build()
        .await
        .unwrap();
    let error = engine
        .create_session(
            AgentProfile::bundled("general-readonly").unwrap(),
            PathBuf::from(root.path().to_string_lossy().to_uppercase()),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "unsafe_workspace");
    engine.close().await.unwrap();
}

#[tokio::test]
async fn stored_session_never_adopts_a_replacement_directory() {
    let parent = tempfile::tempdir().unwrap();
    let workspace = parent.path().join("workspace");
    let database = parent.path().join("sessions.sqlite");
    std::fs::create_dir(&workspace).unwrap();
    let durable = Workspace::open(&workspace)
        .await
        .unwrap()
        .supports_persistent_identity();
    let session_id = {
        let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
            .store(Arc::new(SqliteStore::open(&database).await.unwrap()))
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                AgentProfile::bundled("general-readonly").unwrap(),
                &workspace,
                HostPolicy::default(),
                Value::Null,
            )
            .await
            .unwrap();
        let id = session.id().await;
        engine.close().await.unwrap();
        drop(session);
        drop(engine);
        id
    };
    // All native directory handles have closed, so Windows can rename too.
    std::fs::rename(&workspace, parent.path().join("original")).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
        .store(Arc::new(SqliteStore::open(&database).await.unwrap()))
        .build()
        .await
        .unwrap();
    let error = engine.open_session(&session_id).await.err().unwrap();
    assert_eq!(
        error.code,
        if !durable {
            "workspace_identity_unavailable"
        } else {
            "workspace_conflict"
        }
    );
    engine.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn pinned_admission_rejects_replaced_path_before_saving_a_session() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let pinned = Workspace::open(&root).await.unwrap();
    std::fs::rename(&root, parent.path().join("original")).unwrap();
    std::fs::create_dir(&root).unwrap();
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
        .build()
        .await
        .unwrap();
    let error = engine
        .create_session_in_workspace(
            AgentProfile::bundled("general-readonly").unwrap(),
            pinned,
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "workspace_conflict");
    assert!(engine.list_sessions().await.unwrap().is_empty());
    engine.close().await.unwrap();
}

struct ProcessFixture {
    workspace: Workspace,
    sandbox: Sandbox,
    probe: String,
    // Windows directory handles above must drop before TempDir removes them.
    root: tempfile::TempDir,
}
impl ProcessFixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut sandbox = Sandbox {
            bubblewrap: std::env::var_os("WERK_WORKER_BWRAP")
                .map(Into::into)
                .unwrap_or_else(|| Sandbox::default().bubblewrap),
            native: NativeSandboxConfig {
                macos_path_based_workspace: cfg!(target_os = "macos"),
            },
            ..Default::default()
        };
        if cfg!(target_os = "macos") {
            // Explicit test virtual-address budget, never a production fallback.
            sandbox.limits.memory_bytes = 1024 * 1024 * 1024 * 1024;
        }
        let path = root.path().join("workspace with spaces");
        #[cfg(windows)]
        let workspace = sandbox.create_native_workspace(&path).await.unwrap();
        #[cfg(not(windows))]
        let workspace = {
            std::fs::create_dir(&path).unwrap();
            Workspace::open(&path).await.unwrap()
        };
        let name = if cfg!(windows) { "probe.exe" } else { "probe" };
        let executable = workspace.path.join(name);
        let compiled = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition=2024", "-C", "debuginfo=0"])
            .arg(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/native_process_probe.rs"),
            )
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let probe = if cfg!(target_os = "linux") {
            format!("/workspace/{name}")
        } else {
            executable.to_str().unwrap().to_owned()
        };
        Self {
            root,
            workspace,
            sandbox,
            probe,
        }
    }
}
impl Drop for ProcessFixture {
    fn drop(&mut self) {
        #[cfg(windows)]
        self.sandbox
            .remove_native_workspace_profiles(&self.workspace)
            .unwrap();
    }
}

#[tokio::test]
async fn native_remembered_approval_recovery_requires_durable_identity_and_exact_scope() {
    let fixture = ProcessFixture::new().await;
    let database = fixture.root.path().join("sessions.sqlite");
    let mut session_id: Option<String> = None;
    for pass in 0..4 {
        let mut sandbox = fixture.sandbox.clone();
        if pass == 2 {
            sandbox.limits.timeout_ms -= 1;
        }
        let text = if pass == 3 { "changed" } else { "approved" };
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedTurn::calls(
                "",
                vec![ToolCall {
                    id: id("native-policy-call"),
                    name: "process.run".into(),
                    arguments: json!({"argv":[fixture.probe,"echo",text]}),
                }],
            ),
            ScriptedTurn::text("done"),
        ]));
        let engine = WorkerEngine::builder(provider)
            .sandbox(sandbox.clone())
            .store(Arc::new(SqliteStore::open(&database).await.unwrap()))
            .harness(werk_worker::harness::HarnessConfig {
                coding_checks: false,
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        let session = if let Some(id) = &session_id {
            if !fixture.workspace.supports_persistent_identity() {
                assert_eq!(
                    engine.open_session(id).await.err().unwrap().code,
                    "workspace_identity_unavailable"
                );
                engine.close().await.unwrap();
                // This platform's promised behavior is explicit refusal,
                // after the real first invocation and stored approval above.
                return;
            }
            engine.open_session(id).await.unwrap()
        } else {
            engine
                .create_session_in_workspace(
                    AgentProfile::bundled("coding-basic").unwrap(),
                    fixture.workspace.clone(),
                    HostPolicy {
                        request_approvals: true,
                        workspace_writable: true,
                        tool_permissions: [("process.run".into(), ToolPermission::Ask)].into(),
                        ..Default::default()
                    },
                    Value::Null,
                )
                .await
                .unwrap()
        };
        let run = session
            .submit(
                "Run the native probe",
                &id("submit"),
                Limits::default(),
                None,
            )
            .await
            .unwrap();
        if pass != 1 {
            let mut events = run.events(0);
            let request = tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let event = events
                        .next()
                        .await
                        .expect("run ended without required approval")
                        .unwrap();
                    if let EventKind::ApprovalRequested { request } = event.event {
                        break request;
                    }
                }
            })
            .await
            .expect("changed scope must request a new approval");
            assert_eq!(
                request.description["arguments"]["sandbox"]["contract_version"],
                2
            );
            assert_eq!(
                request.description["arguments"]["sandbox"]["limits"]["timeout_ms"],
                sandbox.limits.timeout_ms
            );
            run.respond_to_approval(
                &request.request_id,
                if pass == 0 {
                    ApprovalDecision::AllowForSession
                } else {
                    ApprovalDecision::AllowOnce
                },
                &id("approve"),
            )
            .await
            .unwrap();
        }
        let done = tokio::time::timeout(Duration::from_secs(20), run.wait())
            .await
            .expect("same approved scope must run after restart without a new prompt")
            .unwrap();
        assert_eq!(done.status, RunStatus::Completed);
        let status = run.status().await.unwrap();
        assert_eq!(status.invocations.len(), 1);
        let invocation = &status.invocations[0];
        assert_eq!(invocation.approval.is_some(), pass != 1);
        let result = invocation.result.as_ref().unwrap();
        assert_eq!(result.status, Outcome::Succeeded, "{:?}", result.data);
        assert_eq!(result.data["exit_code"], 0);
        assert_eq!(result.data["stdout"], format!("{}:{text}\n", text.len()));
        let saved = session.snapshot().await.unwrap();
        assert_eq!(saved.policy.remembered_grants.len(), 1);
        assert!(saved.policy.remembered_grants[0].arguments_hash.is_some());
        session_id = Some(session.id().await);
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn workspace_incarnation_is_stable_across_ordinary_content_changes() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(root.path()).await.unwrap();
    let original = workspace.identity.clone();
    workspace
        .write("content", b"first".to_vec(), None)
        .await
        .unwrap();
    workspace
        .write("content", b"second".to_vec(), Some(hash("first")))
        .await
        .unwrap();
    std::fs::create_dir(root.path().join("nested")).unwrap();
    std::fs::rename(root.path().join("content"), root.path().join("renamed")).unwrap();
    let reopened = Workspace::open(root.path()).await.unwrap();
    assert_eq!(reopened.identity, original);
    assert_eq!(
        reopened.supports_persistent_identity(),
        workspace.supports_persistent_identity()
    );
    workspace.revalidate().await.unwrap();
    if workspace.supports_persistent_identity() {
        assert_eq!(
            workspace.execution_identity(),
            reopened.execution_identity()
        );
    } else {
        assert_ne!(
            workspace.execution_identity(),
            reopened.execution_identity()
        );
        assert_eq!(
            workspace.execution_identity(),
            workspace.clone().execution_identity()
        );
        assert_eq!(
            workspace.require_persistent_identity().unwrap_err().code,
            "workspace_identity_unavailable"
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn recycled_unix_inodes_do_not_recover_previous_session_authority() {
    use std::os::unix::fs::MetadataExt;
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let old_inode = std::fs::metadata(&root).unwrap().ino();
    let database = parent.path().join("sessions.sqlite");
    let (session_id, old_identity, durable) = {
        let workspace = Workspace::open(&root).await.unwrap();
        let durable = workspace.supports_persistent_identity();
        let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
            .store(Arc::new(SqliteStore::open(&database).await.unwrap()))
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session_in_workspace(
                AgentProfile::bundled("general-readonly").unwrap(),
                workspace,
                HostPolicy::default(),
                Value::Null,
            )
            .await
            .unwrap();
        let data = session.snapshot().await.unwrap();
        engine.close().await.unwrap();
        drop(session);
        drop(engine);
        (data.id, data.workspace_identity, durable)
    };
    std::fs::remove_dir(&root).unwrap();
    let mut reused = false;
    let mut attempts = 0;
    for attempt in 1..=512 {
        attempts = attempt;
        std::fs::create_dir(&root).unwrap();
        let current = Workspace::open(&root).await.unwrap();
        assert_eq!(current.supports_persistent_identity(), durable);
        if durable {
            assert_ne!(
                current.identity, old_identity,
                "a newly allocated directory must not inherit durable authority"
            );
        } else {
            assert_eq!(
                current.require_persistent_identity().unwrap_err().code,
                "workspace_identity_unavailable"
            );
        }
        reused = std::fs::metadata(&root).unwrap().ino() == old_inode;
        drop(current);
        if reused || attempt == 512 {
            break;
        }
        std::fs::remove_dir(&root).unwrap();
    }
    eprintln!(
        "inode reuse positive control: reused={reused}, attempts={attempts}, immutable-btime={durable}"
    );
    // Allocation policy is filesystem-specific. Every replacement above was
    // checked even if this host did not recycle the original inode in the bound.
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
        .store(Arc::new(SqliteStore::open(&database).await.unwrap()))
        .build()
        .await
        .unwrap();
    assert_eq!(
        engine.open_session(&session_id).await.err().unwrap().code,
        if durable {
            "workspace_conflict"
        } else {
            "workspace_identity_unavailable"
        }
    );
    engine.close().await.unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn live_only_allocations_cannot_reuse_a_prepared_process_contract() {
    let temp = tempfile::tempdir().unwrap();
    // procfs intentionally exposes no immutable creation time on Linux;
    // macOS lacks a trustworthy unprivileged incarnation on its native FS.
    let path = if cfg!(target_os = "linux") {
        std::path::Path::new("/proc")
    } else {
        temp.path()
    };
    let first = Workspace::open(path).await.unwrap();
    let second = Workspace::open(path).await.unwrap();
    assert!(!first.supports_persistent_identity());
    assert!(!second.supports_persistent_identity());
    assert_eq!(first.identity, second.identity);
    assert_ne!(first.execution_identity(), second.execution_identity());
    let sandbox = Sandbox {
        bubblewrap: std::env::var_os("WERK_WORKER_BWRAP")
            .map(Into::into)
            .unwrap_or_else(|| Sandbox::default().bubblewrap),
        native: NativeSandboxConfig {
            macos_path_based_workspace: cfg!(target_os = "macos"),
        },
        ..Default::default()
    };
    let prepared = sandbox.execution_description(&first, false).await.unwrap();
    sandbox
        .verify_execution_description(&first.clone(), false, &prepared)
        .await
        .unwrap();
    assert_eq!(
        sandbox
            .verify_execution_description(&second, false, &prepared)
            .await
            .unwrap_err()
            .code,
        "sandbox_contract_changed"
    );
}
