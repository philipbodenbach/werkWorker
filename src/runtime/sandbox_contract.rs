//! Host-derived execution contracts are part of the existing approval hash.
use super::*;

pub const CONTRACT_VERSION: u32 = 2;

impl Sandbox {
    /// Describe effective authority without starting a program or changing ACLs.
    pub async fn execution_description(
        &self,
        workspace: &Workspace,
        writable: bool,
    ) -> Result<Value> {
        workspace.revalidate().await?;
        if self.limits.timeout_ms == 0
            || self.limits.memory_bytes == 0
            || self.limits.cpu_seconds == 0
            || self.limits.processes == 0
        {
            return Err(WorkerError::new(
                "invalid_limits",
                "Mandatory process limits must be nonzero",
            ));
        }
        #[cfg(target_os = "linux")]
        let mut description = {
            if !self.bubblewrap.is_absolute() {
                return Err(WorkerError::new(
                    "sandbox_unavailable",
                    "Bubblewrap must be a trusted absolute executable path",
                ));
            }
            let launcher = tokio::fs::canonicalize(&self.bubblewrap)
                .await
                .map_err(|_| {
                    WorkerError::new(
                        "sandbox_unavailable",
                        "Configured Bubblewrap launcher is unavailable",
                    )
                })?;
            let metadata = tokio::fs::metadata(&launcher).await?;
            if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
                return Err(WorkerError::new(
                    "sandbox_unavailable",
                    "Invalid Bubblewrap launcher",
                ));
            }
            let bytes = tokio::fs::read(&launcher).await?;
            json!({
                "backend":"bubblewrap", "launcher":launcher, "launcher_sha256":hash(bytes),
                "workspace_view":"/workspace", "network":false, "ipc":"private namespaces",
                "host_home":false, "runtime_resources":[{"path":"/usr","access":"read-only"}],
                "temporary_resources":"private /tmp", "descendants":"PID namespace teardown",
                "host_exit_cleanup":true, "inode_bound_workspace":true,
                "resource_scope":"per-process rlimits; UID process ceiling, not aggregate cgroup quotas"
            })
        };
        #[cfg(target_os = "macos")]
        let mut description = macos::description(self, workspace, writable)?;
        #[cfg(windows)]
        let mut description = windows::description(workspace, writable, &self.limits)?;
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        let mut description = {
            return Err(WorkerError::new(
                "sandbox_unavailable",
                "No native process backend for this platform",
            ));
            #[allow(unreachable_code)]
            json!({})
        };
        description["contract_version"] = json!(CONTRACT_VERSION);
        description["workspace_identity"] = json!(workspace.identity);
        description["workspace_execution_identity"] = json!(workspace.execution_identity());
        description["persistent_workspace_identity"] =
            json!(workspace.supports_persistent_identity());
        description["workspace_path"] = json!(workspace.path);
        description["workspace_writable"] = json!(writable);
        description["network"] = json!(false);
        description["host_home"] = json!(false);
        description["limits"] = json!(self.limits);
        description["native_configuration"] = json!(self.native);
        Ok(description)
    }

    /// Recheck the exact host-derived contract immediately before dispatch.
    pub async fn verify_execution_description(
        &self,
        workspace: &Workspace,
        writable: bool,
        prepared: &Value,
    ) -> Result<()> {
        if prepared["contract_version"] != CONTRACT_VERSION {
            return Err(WorkerError::new(
                "sandbox_contract_changed",
                "Legacy or missing sandbox contract; prepare and approve the action again",
            ));
        }
        let actual = self.execution_description(workspace, writable).await?;
        if actual != *prepared {
            return Err(WorkerError::new(
                "sandbox_contract_changed",
                "Sandbox backend, workspace, resources or limits changed after preparation",
            ));
        }
        Ok(())
    }

    /// Explicit host operation: create a new dedicated Windows LPAC workspace.
    /// Existing directories are never adopted or recursively re-permissioned.
    pub async fn create_native_workspace(&self, path: impl AsRef<Path>) -> Result<Workspace> {
        #[cfg(windows)]
        {
            windows::create_workspace(path.as_ref()).await
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            Err(WorkerError::new(
                "sandbox_unavailable",
                "Dedicated AppContainer workspace provisioning is a Windows host operation",
            ))
        }
    }

    /// Remove dedicated AppContainer profile storage after closing all workspace engines.
    /// This does not revoke tokens or remove the workspace ACL grants.
    pub fn remove_native_workspace_profiles(&self, workspace: &Workspace) -> Result<()> {
        #[cfg(windows)]
        {
            windows::remove_workspace_profiles(workspace)
        }
        #[cfg(not(windows))]
        {
            let _ = workspace;
            Err(WorkerError::new(
                "sandbox_unavailable",
                "AppContainer profile removal is Windows-only",
            ))
        }
    }

    /// Capability report, including failures. A probe proves only its listed
    /// checks; browser isolation requires its own explicit native tests.
    pub async fn doctor(&self) -> Result<Value> {
        let path = std::env::temp_dir().join(id("werk_doctor"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir(&path)?;
        let files = async {
            let workspace = Workspace::open(&path).await?;
            workspace
                .write("probe", b"synthetic probe".to_vec(), None)
                .await?;
            if workspace.read("probe", 100).await? != b"synthetic probe"
                || workspace.read("../probe", 100).await.is_ok()
            {
                return Err(WorkerError::new(
                    "sandbox_probe_failed",
                    "Workspace confinement probe failed",
                ));
            }
            workspace.list(".", 10).await?;
            Ok::<_, WorkerError>(
                json!({"implemented":true,"available":true,"probe_verified":true,
                "verified":["create-only write","bounded read","listing","traversal rejection"],
                "identity":workspace.identity,
                "persistent_identity":workspace.supports_persistent_identity()}),
            )
        }
        .await;
        #[cfg(target_os = "linux")]
        let process = self.linux_probe().await;
        #[cfg(target_os = "macos")]
        let process = async {
            let workspace = Workspace::open(&path).await?;
            let descriptor = self.execution_description(&workspace, false).await?;
            let result = self
                .run(
                    &workspace,
                    false,
                    &["/usr/bin/true".into()],
                    ".",
                    CancellationToken::new(),
                )
                .await?;
            if result.exit_code != Some(0) || result.timed_out || result.cancelled {
                return Err(WorkerError::new(
                    "sandbox_probe_failed",
                    "Native process startup probe failed",
                ));
            }
            Ok(descriptor)
        }
        .await;
        #[cfg(windows)]
        let process = async {
            let workspace = windows::create_workspace(&path.join("process")).await?;
            let result = async {
                let descriptor = self.execution_description(&workspace, false).await?;
                let result = self
                    .run(
                        &workspace,
                        false,
                        &windows::probe_argv()?,
                        ".",
                        CancellationToken::new(),
                    )
                    .await?;
                if result.exit_code != Some(0) || result.timed_out || result.cancelled {
                    return Err(WorkerError::new(
                        "sandbox_probe_failed",
                        "Native process startup probe failed",
                    ));
                }
                Ok(descriptor)
            }
            .await;
            windows::remove_workspace_profiles(&workspace)?;
            result
        }
        .await;
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        let process: Result<Value> = Err(WorkerError::new(
            "sandbox_unavailable",
            "No native process backend",
        ));
        let file_report = files.unwrap_or_else(|error| json!({"implemented":cfg!(any(target_os="linux",target_os="macos",windows)),"available":false,"probe_verified":false,"error":error}));
        let process_report = match process {
            Ok(description) => json!({"implemented":true,"available":true,"probe_verified":true,
                "verified":["isolated native process startup and exit"],"contract":description}),
            Err(error) => {
                json!({"implemented":cfg!(any(target_os="linux",target_os="macos",windows)),"available":false,"probe_verified":false,"error":error})
            }
        };
        let browser_available = cfg!(target_os = "linux")
            && process_report["available"] == true
            && crate::browser::runtime_path().is_some();
        let browser = json!({"implemented":cfg!(target_os="linux"),"available":browser_available,
            "probe_verified":false,"reason":if cfg!(target_os="linux") {"Requires separate browser runtime and active browser conformance tests"} else {"Native process isolation does not implement isolated browser proxy/network transport"}});
        let _ = std::fs::remove_dir_all(&path);
        Ok(
            json!({"schema_version":2,"capabilities":{"files":file_report,"process":process_report,
            "browser_check":browser,"browser_control":browser},"resource_limits":self.limits}),
        )
    }

    /// Compatibility gate for callers that require an executable process backend.
    pub async fn require_process_backend(&self) -> Result<Value> {
        let report = self.doctor().await?;
        if report["capabilities"]["process"]["available"] != true {
            let error = &report["capabilities"]["process"]["error"];
            return Err(WorkerError::new(
                error["code"].as_str().unwrap_or("sandbox_unavailable"),
                error["message"]
                    .as_str()
                    .unwrap_or("Required native process isolation is unavailable"),
            ));
        }
        Ok(report)
    }
}
