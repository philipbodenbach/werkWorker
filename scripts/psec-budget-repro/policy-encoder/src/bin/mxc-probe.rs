//! Read-only supported-v1 discovery. This never creates a container or workload.
#[cfg(windows)]
fn probe() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    use mxc_sdk::v1::{self, configs::ProcessContainerConfig};
    use serde_json::json;
    let mut arguments = std::env::args().skip(1);
    let root = arguments.next().ok_or("one fresh lab root is required")?;
    if arguments.next().is_some()
        || root.len() < 3
        || !root.as_bytes()[0].is_ascii_alphabetic()
        || &root.as_bytes()[1..3] != b":\\"
    {
        return Err("expected one drive-absolute lab root".into());
    }
    let system = std::env::var("SystemRoot")? + "\\System32";
    let volume = system
        .get(..3)
        .ok_or("drive-qualified SystemRoot required")?
        .to_owned();
    let runtime = format!("{root}\\runtime");
    let workspace = format!("{root}\\workspace");
    let scratch = format!("{root}\\scratch");
    for directory in [&runtime, &workspace, &scratch, &system] {
        if !std::path::Path::new(directory).is_dir() {
            return Err(format!("missing diagnostic directory: {directory}").into());
        }
    }
    // No executable is created: the public probe selects a tier without launch.
    let mut request = v1::ContainerRequest::new(format!("\"{runtime}\\fixture.exe\""));
    request.containment = v1::Containment::ProcessContainer(ProcessContainerConfig::default());
    request.filesystem = Some(v1::FilesystemPolicy {
        readonly_paths: vec![runtime.clone(), system.clone()],
        readwrite_paths: vec![workspace.clone(), scratch.clone()],
        denied_paths: vec![volume.clone()],
        clear_policy_on_exit: Some(true),
    });
    let mut egress = v1::NetworkEgressPolicy::default();
    egress.default = Some(v1::NetworkAction::Deny);
    let mut ingress = v1::NetworkIngressPolicy::default();
    ingress.default = Some(v1::NetworkAction::Deny);
    ingress.host_loopback = Some(v1::NetworkAction::Deny);
    request.network = Some(v1::NetworkPolicy {
        egress: Some(egress),
        ingress: Some(ingress),
        runtime_config: None,
    });
    request.ui = Some(v1::UiPolicy::default());
    request.timeout_ms = Some(30_000);
    request.working_directory = Some(workspace.clone());
    request.environment = Some(Vec::new());
    request.inherit_default_environment = Some(false);
    let observation = v1::probe(Some(&request))?;
    let prerequisite = observation.tier == Some("base-container")
        && observation.needs_dacl_augmentation == Some(false)
        && observation.error.is_none()
        && observation.warnings.is_empty();
    Ok(json!({
        "sdk": "mxc-sdk=1.0.1", "sdk_default_features": false,
        "probe_only": true, "workload_launched": false, "sandbox_qualified": false,
        "request": {"containment": "ProcessContainer", "explicit_capabilities": [],
            "readonly": [runtime, system], "readwrite": [workspace, scratch], "deny": [volume],
            "network": {"egress": "deny", "ingress": "deny", "host_loopback": "deny"},
            "ui_disabled": true, "environment": [], "inherit_default_environment": false},
        "probe": observation, "exact_base_container_prerequisite": prerequisite,
        "unmet_supported_api_contract": ["caller-owned Job before execution",
            "finite aggregate process/CPU/memory limits", "before-resume verification",
            "confirmed whole-Job cleanup"],
    }))
}

fn main() {
    #[cfg(windows)]
    let result = probe();
    #[cfg(not(windows))]
    let result: Result<serde_json::Value, Box<dyn std::error::Error>> =
        Err("native Windows execution is required".into());
    let (report, status) = match result {
        Ok(report) => {
            let status = if report["exact_base_container_prerequisite"] == true {
                0
            } else {
                2
            };
            (report, status)
        }
        Err(error) => (
            serde_json::json!({"probe_only": true, "workload_launched": false,
            "sandbox_qualified": false, "exact_base_container_prerequisite": false,
            "error": error.to_string()}),
            2,
        ),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("JSON report")
    );
    std::process::exit(status);
}
