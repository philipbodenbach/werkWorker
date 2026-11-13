//! Full shared engine path: observed edit, exact approval, native process and
//! current-file verification. The source fixture is a tiny interpreted expression.
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use werk_worker::{policy::*, profiles::AgentProfile, providers::*, runtime::*, *};

#[tokio::test]
async fn native_scripted_coding_cycle_uses_real_files_process_and_verification() {
    let root = tempfile::tempdir().unwrap();
    let sandbox = Sandbox {
        bubblewrap: std::env::var_os("WERK_WORKER_BWRAP")
            .map(Into::into)
            .unwrap_or_else(|| Sandbox::default().bubblewrap),
        native: NativeSandboxConfig {
            macos_path_based_workspace: cfg!(target_os = "macos"),
        },
        // Darwin reserves large virtual mappings. This is a finite AS bound,
        // not evidence of a practical resident-memory quota.
        limits: ProcessLimits {
            memory_bytes: if cfg!(target_os = "macos") {
                1 << 40
            } else {
                1 << 30
            },
            ..Default::default()
        },
    };
    #[cfg(windows)]
    let workspace = sandbox
        .create_native_workspace(root.path().join("workspace"))
        .await
        .unwrap();
    #[cfg(not(windows))]
    let workspace = Workspace::open(root.path()).await.unwrap();
    let program_name = if cfg!(windows) {
        "native-probe.exe"
    } else {
        "native-probe"
    };
    let program = workspace.path.join(program_name);
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_process_probe.rs");
    let compilation =
        std::process::Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition=2024", "-C", "debuginfo=0"])
            .arg(&source)
            .arg("-o")
            .arg(&program)
            .output()
            .unwrap();
    assert!(
        compilation.status.success(),
        "{}",
        String::from_utf8_lossy(&compilation.stderr)
    );
    workspace
        .write("add.expr", b"a-b\n".to_vec(), None)
        .await
        .unwrap();
    workspace
        .write("checks.csv", b"2,3,5\n-2,3,1\n0,0,0\n".to_vec(), None)
        .await
        .unwrap();
    let control = std::process::Command::new(&program)
        .args(["test-add", "add.expr", "checks.csv"])
        .current_dir(&workspace.path)
        .output()
        .unwrap();
    assert_eq!(
        control.status.code(),
        Some(101),
        "The native assertion must reject the broken expression: {control:?}"
    );
    assert!(
        String::from_utf8_lossy(&control.stderr).contains("addition behavior for 2,3,5"),
        "The expected arithmetic assertion must run before repair: {control:?}"
    );
    let native_program = if cfg!(target_os = "linux") {
        format!("/workspace/{program_name}")
    } else {
        program.to_string_lossy().into_owned()
    };
    let call = |name: &str, arguments: Value| ToolCall {
        id: id("call"),
        name: name.into(),
        arguments,
    };
    let plan = |status: &str| {
        call(
            "task.plan",
            json!({"steps":[{
        "id":"addition", "description":"Correct addition and verify native behavior", "status":status,
        "acceptance_criteria":["addition passes the three numeric cases"],
        "verification_method":"Run the native expression check in the sandbox"}]}),
        )
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::calls(
            "Read the current source and test cases",
            vec![
                plan("in_progress"),
                call("files.find", json!({"path":".","pattern":"**/*.csv"})),
                call("files.read", json!({"path":"add.expr"})),
                call("files.read", json!({"path":"checks.csv"})),
            ],
        ),
        ScriptedTurn::calls(
            "Fix the observed arithmetic expression",
            vec![call(
                "files.edit",
                json!({"path":"add.expr","old":"a-b","new":"a+b","expected_hash":hash("a-b\n")}),
            )],
        ),
        ScriptedTurn::calls(
            "Execute the native behavior check",
            vec![call(
                "process.run",
                json!({
            "argv":[native_program,"test-add","add.expr","checks.csv"],"cwd":".",
            "verification":{"kind":"behavior","criteria":["addition passes the three numeric cases"],
            "paths":["add.expr","checks.csv"]}}),
            )],
        ),
        ScriptedTurn::calls("Record verified completion", vec![plan("completed")]),
        ScriptedTurn::text("Addition repaired; three native behavior checks passed."),
    ]));
    let engine = WorkerEngine::builder(provider)
        .sandbox(sandbox.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                // Keep this process/file conformance fixture independent of
                // whether optional browser tools are installed on the host.
                tools: [
                    "task.plan",
                    "files.find",
                    "files.read",
                    "files.edit",
                    "process.run",
                ]
                .map(str::to_owned)
                .to_vec(),
                ..AgentProfile::bundled("coding-basic").unwrap()
            },
            &workspace.path,
            HostPolicy {
                request_approvals: true,
                workspace_writable: true,
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Fix addition and verify the changed source",
            "native-coding",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    let mut approvals = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut events = run.events(0);
        while let Some(event) = events.next().await {
            if let EventKind::ApprovalRequested { request } = event.unwrap().event {
                let tool = request.description["tool"].as_str().unwrap().to_owned();
                assert!(matches!(
                    tool.as_str(),
                    "files.find" | "files.read" | "files.edit" | "process.run"
                ));
                if tool == "process.run" {
                    assert_eq!(
                        request.description["arguments"]["sandbox"]["workspace_identity"],
                        workspace.identity
                    );
                    assert_eq!(
                        request.description["arguments"]["sandbox"]["contract_version"],
                        2
                    );
                }
                approvals.push(tool);
                run.respond_to_approval(
                    &request.request_id,
                    ApprovalDecision::AllowOnce,
                    &id("approve-native"),
                )
                .await
                .unwrap();
            }
        }
    })
    .await
    .expect("Native coding smoke must finish with bounded lifecycle");
    let terminal = run.wait().await.unwrap();
    assert_eq!(terminal.status, RunStatus::Completed, "{terminal:#?}");
    let state = run.status().await.unwrap();
    let discovery = state
        .invocations
        .iter()
        .find(|i| i.call.name == "files.find")
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert_eq!(discovery.status, Outcome::Succeeded);
    assert_eq!(discovery.data["paths"], json!(["checks.csv"]));
    let invocation = state
        .invocations
        .iter()
        .find(|i| i.call.name == "process.run")
        .unwrap();
    let output = invocation.result.as_ref().unwrap();
    assert_eq!(output.status, Outcome::Succeeded, "{:?}", output.data);
    assert_eq!(output.data["exit_code"], 0);
    assert!(
        output.data["stdout"]
            .as_str()
            .unwrap()
            .contains("3 checks passed")
    );
    assert!(
        output.data.get("verification").is_some(),
        "Current-file verification receipt is required"
    );
    assert!(output.data.get("verification_rejected").is_none());
    assert_eq!(workspace.read("add.expr", 100).await.unwrap(), b"a+b\n");
    assert!(approvals.iter().any(|tool| tool == "files.edit"));
    assert!(approvals.iter().any(|tool| tool == "process.run"));
    engine.close().await.unwrap();
    #[cfg(windows)]
    sandbox
        .remove_native_workspace_profiles(&workspace)
        .unwrap();
}
