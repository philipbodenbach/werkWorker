//! Approval contracts must bind effective host authority before any execution.
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use werk_worker::{policy::*, runtime::*, tools::*, *};

struct Fixture {
    context: ToolContext,
    _directory: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let sandbox = Sandbox {
            bubblewrap: std::env::var_os("WERK_WORKER_BWRAP")
                .map(Into::into)
                .unwrap_or_else(|| Sandbox::default().bubblewrap),
            native: NativeSandboxConfig {
                macos_path_based_workspace: cfg!(target_os = "macos"),
            },
            ..Default::default()
        };
        #[cfg(windows)]
        let workspace = sandbox
            .create_native_workspace(directory.path().join("workspace"))
            .await
            .unwrap();
        #[cfg(not(windows))]
        let workspace = Workspace::open(directory.path()).await.unwrap();
        Self {
            context: ToolContext {
                workspace,
                workspace_lease: None,
                writable: true,
                sandbox,
                session_id: "session_contract".into(),
                run_id: "run_contract".into(),
                invocation_id: "invocation_contract".into(),
                cancel: CancellationToken::new(),
                jobs: Arc::new(JobManager::default()),
                artifacts: None,
                artifact_refs: vec![],
                skills: Default::default(),
                native_jobs: vec![],
            },
            _directory: directory,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        #[cfg(windows)]
        self.context
            .sandbox
            .remove_native_workspace_profiles(&self.context.workspace)
            .unwrap();
    }
}

#[tokio::test]
async fn prepared_execution_rejects_permission_limits_workspace_and_legacy_changes() {
    let mut fixture = Fixture::new().await;
    let tool = ToolRegistry::builtins()
        .unwrap()
        .get("process.run")
        .unwrap();
    let prepared = tool
        .prepare(json!({"argv":["never-executed"]}), &fixture.context)
        .await
        .unwrap();
    assert_eq!(prepared["sandbox"]["contract_version"], 2);
    assert_eq!(
        prepared["sandbox"]["workspace_identity"],
        fixture.context.workspace.identity
    );
    for change in ["rights", "limits", "legacy", "missing", "identity"] {
        let mut args = prepared.clone();
        fixture.context.writable = true;
        fixture.context.sandbox.limits.timeout_ms = 30_000;
        match change {
            "rights" => fixture.context.writable = false,
            "limits" => fixture.context.sandbox.limits.timeout_ms = 29_999,
            "legacy" => args["sandbox"] = json!({"backend":"bubblewrap","workspace_writable":true}),
            "missing" => {
                args.as_object_mut().unwrap().remove("sandbox");
            }
            "identity" => args["sandbox"]["workspace_identity"] = json!("another-object"),
            _ => unreachable!(),
        }
        assert_eq!(
            tool.execute(args, &fixture.context).await.unwrap_err().code,
            "sandbox_contract_changed",
            "{change}"
        );
    }
}

#[tokio::test]
async fn reusable_process_approvals_require_complete_arguments_and_contract() {
    let fixture = Fixture::new().await;
    let tool = ToolRegistry::builtins()
        .unwrap()
        .get("process.run")
        .unwrap();
    let prepared = tool
        .prepare(json!({"argv":["never-executed","first"]}), &fixture.context)
        .await
        .unwrap();
    let definition = tool.definition();
    let mut policy = HostPolicy {
        request_approvals: true,
        workspace_writable: true,
        remembered_grants: vec![CapabilityGrant {
            tool: definition.name.clone(),
            version: definition.version.clone(),
            resource: "workspace".into(),
            arguments_hash: Some(fingerprint(&prepared)),
            expires_at_ms: None,
            preapproved: true,
        }],
        ..Default::default()
    };
    assert_eq!(
        policy.evaluate(&definition, &prepared, &[]),
        PolicyDecision::Allow
    );
    for field in ["argv", "stdin", "sandbox"] {
        let mut changed = prepared.clone();
        changed[field] = match field {
            "argv" => json!(["never-executed", "other"]),
            "stdin" => json!("different input"),
            _ => {
                let mut value = changed["sandbox"].clone();
                value["backend"] = json!("other");
                value
            }
        };
        assert_eq!(
            policy.evaluate(&definition, &changed, &[]),
            PolicyDecision::Ask,
            "{field}"
        );
    }
    policy.remembered_grants[0].arguments_hash = None;
    assert_eq!(
        policy.evaluate(&definition, &prepared, &[]),
        PolicyDecision::Ask
    );
    policy.hard_denied_tools.push(definition.name.clone());
    assert_eq!(
        policy.evaluate(&definition, &prepared, &[]),
        PolicyDecision::Deny
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn doctor_keeps_file_capability_when_process_launcher_is_missing() {
    let sandbox = Sandbox {
        bubblewrap: "/nonexistent/werkWorker-bwrap".into(),
        ..Default::default()
    };
    let report = sandbox.doctor().await.unwrap();
    assert_eq!(report["capabilities"]["files"]["available"], true);
    assert_eq!(report["capabilities"]["files"]["probe_verified"], true);
    assert_eq!(report["capabilities"]["process"]["available"], false);
    assert_eq!(report["capabilities"]["process"]["probe_verified"], false);
    assert_eq!(
        report["capabilities"]["browser_control"]["probe_verified"],
        false
    );
    assert!(sandbox.require_process_backend().await.is_err());
}
