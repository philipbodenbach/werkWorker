//! Offline scripted coding cycle, with real approved sandbox test execution.
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{policy::*, profiles::AgentProfile, providers::*, runtime::Sandbox, *};

#[tokio::main]
async fn main() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let before = include_str!("fixtures/coding/math_helper.py");
    tokio::fs::write(workspace.path().join("math_helper.py"), before).await?;
    tokio::fs::write(
        workspace.path().join("AGENTS.md"),
        "Keep the change small. Verify addition with test_math_helper.py.\n",
    )
    .await?;
    tokio::fs::write(
        workspace.path().join("test_math_helper.py"),
        include_str!("fixtures/coding/test_math_helper.py"),
    )
    .await?;
    let call = |name: &str, arguments: Value| ToolCall {
        id: id("call"),
        name: name.into(),
        arguments,
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::calls(
            "Read the broken function.",
            vec![
                call("skills.load", json!({"name":"coding-basic"})),
                call("files.read", json!({"path":"AGENTS.md"})),
                call("files.read", json!({"path":"math_helper.py"})),
            ],
        ),
        ScriptedTurn::calls(
            "Use addition on the observed base.",
            vec![call(
                "files.edit",
                json!({"path":"math_helper.py","old":"return a - b","new":"return a + b","expected_hash":hash(before)}),
            )],
        ),
        ScriptedTurn::calls(
            "Run the relevant checks.",
            vec![call(
                "process.run",
                json!({"argv":["/usr/bin/python3","test_math_helper.py"],"cwd":".","verification":{"criteria":["addition returns correct sums"],"paths":["math_helper.py","test_math_helper.py"]}}),
            )],
        ),
        ScriptedTurn::text(
            "Changed subtraction to addition; the sandbox test reports two passing checks.",
        ),
    ]));
    let sandbox = Sandbox {
        bubblewrap: std::env::var_os("WERK_WORKER_BWRAP")
            .map(Into::into)
            .unwrap_or_else(|| "/usr/bin/bwrap".into()),
        ..Default::default()
    };
    sandbox.doctor().await?;
    let engine = WorkerEngine::builder(provider)
        .sandbox(sandbox)
        .build()
        .await?;
    let grants = [
        ("files.read", "."),
        ("files.edit", "."),
        ("process.run", "workspace"),
    ]
    .into_iter()
    .map(|(tool, resource)| CapabilityGrant {
        tool: tool.into(),
        version: "1".into(),
        resource: resource.into(),
        arguments_hash: None,
        expires_at_ms: None,
        preapproved: false,
    })
    .collect();
    let session = engine
        .create_session(
            AgentProfile::bundled("coding-basic")?,
            workspace.path(),
            HostPolicy {
                grants,
                workspace_writable: true,
                ..Default::default()
            },
            Value::Null,
        )
        .await?;
    let run = session
        .submit(
            "Fix add and run its relevant test",
            "coding-demo",
            Limits::default(),
            None,
        )
        .await?;
    let mut events = run.events(0);
    while let Some(event) = events.next().await {
        let event = event?;
        println!("{}", serde_json::to_string(&event)?);
        if let EventKind::ApprovalRequested { request } = event.event {
            // This deterministic demo host authorizes exactly the fixture actions
            // shown above. Production hosts should render the request to a user.
            run.respond_to_approval(
                &request.request_id,
                ApprovalDecision::AllowOnce,
                &id("demo-approval"),
            )
            .await?;
        }
    }
    let result = run.wait().await?;
    assert_eq!(result.status, RunStatus::Completed);
    let status = run.status().await?;
    let test = status
        .invocations
        .iter()
        .find(|i| i.call.name == "process.run")
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert_eq!(test.data["exit_code"], 0);
    assert!(
        tokio::fs::read_to_string(workspace.path().join("math_helper.py"))
            .await?
            .contains("a + b")
    );
    println!("{}", serde_json::to_string(&result)?);
    engine.close().await
}
