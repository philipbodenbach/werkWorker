use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use werk_worker::{policy::*, profiles::AgentProfile, providers::*, runtime::*, *};

async fn sandbox() -> Option<Sandbox> {
    let sandbox = Sandbox::default();
    if let Err(error) = sandbox.doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        eprintln!("SKIP real stdin isolation: {error}");
        return None;
    }
    Some(sandbox)
}

#[tokio::test]
async fn literal_stdin_delivers_eof_without_interpolation_and_keeps_limits() {
    let Some(mut sandbox) = sandbox().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(dir.path()).await.unwrap();
    let payload = format!("$(touch escaped)\n`touch escaped2`\n{}", "é\n".repeat(9000));
    let out = sandbox
        .run_with_stdin(
            &workspace,
            true,
            &["cat".into()],
            ".",
            CancellationToken::new(),
            &payload,
        )
        .await
        .unwrap();
    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.stdout, payload);
    assert!(!dir.path().join("escaped").exists());
    assert!(!dir.path().join("escaped2").exists());
    let too_large = "é".repeat(16385);
    assert_eq!(
        sandbox
            .run_with_stdin(
                &workspace,
                true,
                &["cat".into()],
                ".",
                CancellationToken::new(),
                &too_large
            )
            .await
            .unwrap_err()
            .code,
        "invalid_stdin"
    );
    // A child may exit before consuming input: this must not become an error
    // or leave a task blocked on a full stdin pipe.
    let out = sandbox
        .run_with_stdin(
            &workspace,
            false,
            &["true".into()],
            ".",
            CancellationToken::new(),
            &payload,
        )
        .await
        .unwrap();
    assert_eq!(out.exit_code, Some(0));
    sandbox.limits.timeout_ms = 200;
    let argv = ["sh".into(), "-c".into(), "sleep 10".into()];
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        sandbox.run_with_stdin(
            &workspace,
            false,
            &argv,
            ".",
            CancellationToken::new(),
            &payload,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(out.timed_out);
    sandbox.limits.timeout_ms = 10_000;
    let cancel = CancellationToken::new();
    let cancellation = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    };
    let (result, _) = tokio::join!(
        sandbox.run_with_stdin(&workspace, false, &argv, ".", cancel.clone(), &payload),
        cancellation
    );
    assert!(result.unwrap().cancelled);
}

#[tokio::test]
async fn malformed_check_recovers_in_one_corrected_call_with_input_bound_to_approval() {
    let Some(sandbox) = sandbox().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("counter.sh"),
        "increment() { echo $(($1 + 1)); }\n",
    )
    .unwrap();
    let script = ". ./counter.sh\ntest \"$(increment 0)\" = 1\n";
    let turn = |arguments| {
        ScriptedTurn::calls(
            "",
            vec![ToolCall {
                id: id("call"),
                name: "process.run".into(),
                arguments,
            }],
        )
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        turn(
            json!({"argv":["python3","- <<'PY'\nprint(1)\nPY"],"verification":{"criteria":["Inspect counter"],"paths":["counter.sh"]}}),
        ),
        turn(
            json!({"argv":["sh"],"stdin":script,"verification":{"kind":"behavior","criteria":["increment(0) is 1"],"paths":["counter.sh"]}}),
        ),
        ScriptedTurn::text("Verified increment"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .sandbox(sandbox)
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "coding-basic".into(),
                prompt: "Verify behavior".into(),
                tools: vec!["process.run".into()],
                skills: vec![],
            },
            dir.path(),
            HostPolicy {
                tool_permissions: [("process.run".into(), ToolPermission::Ask)].into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("Check increment", "start", Limits::continuous(), None)
        .await
        .unwrap();
    let mut events = run.events(0);
    let request = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(event) = events.next().await {
            if let EventKind::ApprovalRequested { request } = event.unwrap().event {
                return request;
            }
        }
        panic!("Expected approval for the corrected command");
    })
    .await
    .unwrap();
    assert_eq!(request.description["arguments"]["stdin"], script);
    assert_eq!(request.description["arguments"]["argv"], json!(["sh"]));
    let definition = engine
        .tools()
        .into_iter()
        .find(|d| d.name == "process.run")
        .unwrap();
    let mut other_input = request.description["arguments"].clone();
    other_input["stdin"] = json!("touch unexpected");
    assert_ne!(
        request.arguments_hash,
        approval_key(&definition, &other_input)
    );
    run.respond_to_approval(
        &request.request_id,
        ApprovalDecision::AllowOnce,
        "allow-corrected",
    )
    .await
    .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    let recorded = run.status().await.unwrap();
    let invalid = &recorded.invocations[0];
    assert!(invalid.approval.is_none());
    assert_eq!(
        invalid.result.as_ref().unwrap().error_code.as_deref(),
        Some("invalid_process_invocation")
    );
    assert!(
        invalid.result.as_ref().unwrap().data["message"]
            .as_str()
            .unwrap()
            .contains("stdin")
    );
    assert_eq!(
        recorded.invocations[1].result.as_ref().unwrap().data["verification"]["passed"],
        true
    );
    assert_eq!(provider.requests().len(), 3); // Invalid proposal, corrected check, final.
    assert!(!dir.path().join("unexpected").exists());
    engine.close().await.unwrap();
}
