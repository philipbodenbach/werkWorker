use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{context::validate_messages, policy::*, profiles::AgentProfile, providers::*, *};

fn call(name: &str, arguments: Value) -> ScriptedTurn {
    ScriptedTurn::calls(
        "",
        vec![ToolCall {
            id: id("call"),
            name: name.into(),
            arguments,
        }],
    )
}

fn truncated() -> ScriptedTurn {
    ScriptedTurn {
        deltas: vec![
            ModelDelta::Text {
                text: "Creating the entire game now".into(),
            },
            // Even a complete call in a truncated batch must never execute.
            ModelDelta::Tool {
                index: 0,
                id: "discarded".into(),
                name: "files.write".into(),
                arguments: json!({"path":"must-not-exist.html","content":"discarded"}).to_string(),
            },
            ModelDelta::Tool {
                index: 1,
                id: "partial".into(),
                name: "files.write".into(),
                arguments: r#"{"path":"game.html","content":"unfinished"#.into(),
            },
            ModelDelta::Finish {
                reason: "length".into(),
            },
            ModelDelta::Done,
        ],
        delay_ms: 0,
        error: None,
    }
}

fn profile() -> AgentProfile {
    AgentProfile {
        schema_version: 1,
        name: "coding".into(),
        prompt: "Implement the requested file".into(),
        tools: vec!["files.write".into(), "files.edit".into()],
        skills: vec![],
    }
}

#[tokio::test]
async fn repeated_tests_after_workspace_changes_are_allowed_but_no_progress_is_bounded() {
    let sandbox = runtime::Sandbox::default();
    if let Err(error) = sandbox.doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        eprintln!("SKIP real sandbox: {error}");
        return;
    }
    for progress in [true, false] {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("counter"), "one").unwrap();
        let check = || {
            call(
                "process.run",
                json!({"argv":["/bin/cat","counter"],"cwd":"/workspace"}),
            )
        };
        let mut turns = vec![check()];
        if progress {
            turns.extend([
                call(
                    "files.write",
                    json!({
                        "path":"counter","content":"two","expected_hash":hash("one")
                    }),
                ),
                check(),
                call(
                    "process.run",
                    json!({"argv":["/bin/sh","-c","printf three > counter"]}),
                ),
            ]);
        } else {
            // A new read is evidence once; unchanged reads and failed writes
            // must not disguise an unproductive loop.
            turns.extend([
                call("files.read", json!({"path":"counter"})),
                check(),
                call("files.read", json!({"path":"counter"})),
                call(
                    "files.write",
                    json!({
                        "path":"counter","content":"bad","expected_hash":hash("stale")
                    }),
                ),
            ]);
        }
        turns.extend([check(), ScriptedTurn::text("Verified.")]);
        let provider = Arc::new(ScriptedProvider::new(turns));
        let engine = WorkerEngine::builder(provider).build().await.unwrap();
        let mut agent = profile();
        agent
            .tools
            .extend(["process.run".into(), "files.read".into()]);
        let session = engine
            .create_session(
                agent,
                workspace.path(),
                HostPolicy {
                    workspace_writable: true,
                    grants: ["files.write", "files.read", "process.run"]
                        .into_iter()
                        .map(|tool| CapabilityGrant {
                            tool: tool.into(),
                            version: "1".into(),
                            resource: if tool == "process.run" {
                                "workspace"
                            } else {
                                "."
                            }
                            .into(),
                            arguments_hash: None,
                            expires_at_ms: None,
                            preapproved: true,
                        })
                        .collect(),
                    ..Default::default()
                },
                Value::Null,
            )
            .await
            .unwrap();
        let run = session
            .submit(
                "Edit and verify",
                "start",
                Limits {
                    max_repeated_calls: 1,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        if progress {
            assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
            let state = session.snapshot().await.unwrap();
            let outputs: Vec<_> = state.runs[0]
                .invocations
                .iter()
                .filter(|i| {
                    i.call.arguments == json!({"argv":["/bin/cat","counter"],"cwd":"/workspace"})
                })
                .map(|i| i.result.as_ref().unwrap().data["stdout"].as_str().unwrap())
                .collect();
            assert_eq!(outputs, ["one", "two", "three"]);
        } else {
            assert_eq!(result.error.unwrap().code, "repetition_limit");
        }
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn workspace_aliases_support_long_edit_validate_repair_cycles() {
    let sandbox = runtime::Sandbox::default();
    if let Err(error) = sandbox.doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let mut turns = vec![
        call(
            "process.run",
            json!({"argv":["mkdir","-p","/workspace/src"]}),
        ),
        call(
            "files.write",
            json!({"path":"/workspace/src/counter","content":"0"}),
        ),
        call("files.list", json!({"path":"/workspace/src"})),
        call("files.search", json!({"path":"/workspace/src","query":"0"})),
    ];
    let mut previous = "0".to_string();
    for index in 1..=12 {
        // The fourth test fails, then the next edit repairs it. Every test uses
        // exactly the same argv and must be allowed after each file revision.
        let next = if index == 4 {
            "-4".to_string()
        } else {
            index.to_string()
        };
        turns.extend([
            call("files.read", json!({"path":"/workspace/src/counter"})),
            call("files.edit", json!({"path":"/workspace/src/counter", "old":previous,
                "new":next,"expected_hash":hash(&previous)})),
            call("process.run", json!({"argv":["/bin/sh","-c",
                "value=$(cat /workspace/src/counter); printf '%s\\n' \"$value\"; test \"$value\" -ge 0"],"cwd":"/workspace/src"})),
        ]);
        previous = next;
    }
    turns.push(ScriptedTurn::text("Verified after repair."));
    let provider = Arc::new(ScriptedProvider::new(turns));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let names = [
        "files.write",
        "files.edit",
        "files.read",
        "files.list",
        "files.search",
        "process.run",
    ];
    let mut agent = profile();
    agent.tools = names.iter().map(|s| s.to_string()).collect();
    let policy = HostPolicy {
        workspace_writable: true,
        grants: names
            .iter()
            .map(|name| CapabilityGrant {
                tool: name.to_string(),
                version: "1".into(),
                resource: if *name == "process.run" {
                    "workspace"
                } else {
                    "src"
                }
                .into(),
                arguments_hash: None,
                expires_at_ms: None,
                preapproved: true,
            })
            .collect(),
        ..Default::default()
    };
    let session = engine
        .create_session(agent, workspace.path(), policy, Value::Null)
        .await
        .unwrap();
    let run = session
        .submit(
            "Implement, validate and repair",
            "start",
            Limits {
                max_repeated_calls: 1,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(45), run.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(result.metrics.steps, 41);
    let state = session.snapshot().await.unwrap();
    let invocations = &state.runs[0].invocations;
    for invocation in invocations {
        assert_eq!(
            invocation.result.as_ref().unwrap().status,
            Outcome::Succeeded,
            "{:?}",
            invocation.result
        );
        if invocation.call.name.starts_with("files.") {
            assert!(
                invocation.prepared["path"]
                    .as_str()
                    .unwrap()
                    .starts_with("src")
            );
        }
    }
    let checks: Vec<_> = invocations
        .iter()
        .filter(|i| i.call.name == "process.run")
        .skip(1)
        .map(|i| &i.result.as_ref().unwrap().data)
        .collect();
    assert_eq!(checks.len(), 12);
    for (index, check) in checks.iter().enumerate() {
        assert_eq!(check["exit_code"], if index == 3 { 1 } else { 0 });
        assert_eq!(
            check["stdout"],
            format!(
                "{}\n",
                if index == 3 {
                    "-4".into()
                } else {
                    (index + 1).to_string()
                }
            )
        );
    }
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("src/counter")).unwrap(),
        "12"
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn file_conflicts_explain_stale_hash_and_missing_match_without_writing() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("game.html"), "<!--SCRIPT-->").unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        call(
            "files.edit",
            json!({
                "path":"game.html", "expected_hash":hash("old"),
                "old":"<!--SCRIPT-->", "new":"must not run"
            }),
        ),
        call(
            "files.edit",
            json!({
                "path":"game.html", "expected_hash":hash("<!--SCRIPT-->"),
                "old":"<!--MISSING-->", "new":"must not run"
            }),
        ),
        call(
            "files.write",
            json!({"path":"game.html","content":"must not overwrite"}),
        ),
        ScriptedTurn::text("Read the current insertion target before retrying."),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(),
            workspace.path(),
            HostPolicy {
                workspace_writable: true,
                grants: ["files.edit", "files.write"]
                    .into_iter()
                    .map(|tool| CapabilityGrant {
                        tool: tool.into(),
                        version: "1".into(),
                        resource: ".".into(),
                        arguments_hash: None,
                        expires_at_ms: None,
                        preapproved: true,
                    })
                    .collect(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("Edit the file", "start", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("game.html")).unwrap(),
        "<!--SCRIPT-->"
    );
    let requests = provider.requests();
    for (index, expected) in [
        (1, "File hash changed"),
        (2, "one unique nonempty match"),
        (3, "Target already exists; supply expected_hash"),
    ] {
        let message = requests[index]
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .unwrap();
        let result: ToolResult = serde_json::from_str(&message.content).unwrap();
        assert_eq!(result.error_code.as_deref(), Some("write_conflict"));
        assert!(result.data["message"].as_str().unwrap().contains(expected));
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn truncated_generation_recovers_into_one_file_without_user_steering() {
    let workspace = tempfile::tempdir().unwrap();
    let skeleton = format!(
        "<html><body>{}<!--STYLE--><!--SCRIPT--></body></html>",
        "scene ".repeat(700)
    );
    let styled = skeleton.replace(
        "<!--STYLE-->",
        &format!("<style>/*{}*/</style>", "style ".repeat(700)),
    );
    let complete = styled.replace(
        "<!--SCRIPT-->",
        &format!("<script>/*{}*/</script>", "logic ".repeat(700)),
    );
    let provider = Arc::new(ScriptedProvider::new(vec![
        truncated(),
        call(
            "files.write",
            json!({"path":"game.html","content":skeleton}),
        ),
        call(
            "files.edit",
            json!({"path":"game.html","expected_hash":hash(&skeleton),
            "old":"<!--STYLE-->","new":format!("<style>/*{}*/</style>", "style ".repeat(700))}),
        ),
        call(
            "files.edit",
            json!({"path":"game.html","expected_hash":hash(&styled),
            "old":"<!--SCRIPT-->","new":format!("<script>/*{}*/</script>", "logic ".repeat(700))}),
        ),
        ScriptedTurn::text("The HTML file is complete."),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let policy = HostPolicy {
        workspace_writable: true,
        grants: ["files.write", "files.edit"]
            .into_iter()
            .map(|tool| CapabilityGrant {
                tool: tool.into(),
                version: "1".into(),
                resource: ".".into(),
                arguments_hash: None,
                expires_at_ms: None,
                preapproved: false,
            })
            .collect(),
        ..Default::default()
    };
    let session = engine
        .create_session(profile(), workspace.path(), policy, Value::Null)
        .await
        .unwrap();
    let run = session
        .submit(
            "Create one HTML game",
            "start",
            Limits {
                reserve_output_tokens: 4096,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let mut approvals = 0;
    let mut events = run.events(0);
    while let Some(event) = events.next().await {
        if let EventKind::ApprovalRequested { request } = event.unwrap().event {
            assert_eq!(request.description["arguments"]["path"], "game.html");
            assert!(!workspace.path().join("must-not-exist.html").exists());
            approvals += 1;
            run.respond_to_approval(
                &request.request_id,
                ApprovalDecision::AllowOnce,
                &id("approve"),
            )
            .await
            .unwrap();
        }
    }
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(approvals, 3, "normal write approvals must remain in force");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("game.html")).unwrap(),
        complete
    );
    assert!(!workspace.path().join("must-not-exist.html").exists());
    let state = run.status().await.unwrap();
    assert_eq!(state.metrics.model_requests, 5);
    assert_eq!(state.invocations.len(), 3);
    for invocation in &state.invocations {
        let receipt = invocation.result.as_ref().unwrap();
        assert!(!receipt.truncated);
        assert_eq!(receipt.data["sha256"].as_str().unwrap().len(), 64);
        assert!(
            receipt.data.to_string().len() < 300,
            "growing file bodies must not flood context"
        );
    }
    let requests = provider.requests();
    assert!(
        !requests[0].messages[0]
            .content
            .contains("Build large files across turns")
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.role == Role::Assistant
                && m.content.contains("\"event\":\"generation_output_limit\""))
    );
    assert_ne!(fingerprint(&requests[0]), fingerprint(&requests[1]));
    assert_eq!(requests[0].messages[0], requests[1].messages[0]);
    assert!(
        requests
            .last()
            .unwrap()
            .messages
            .iter()
            .any(|m| m.role != Role::System && m.content.contains(&hash(&complete)))
    );
    for request in requests {
        assert_eq!(request.max_output_tokens, 4096);
        validate_messages(&request.messages).unwrap();
        assert!(
            !request
                .messages
                .iter()
                .any(|m| m.content.contains("Creating the entire game now"))
        );
        assert!(
            !request
                .messages
                .iter()
                .flat_map(|m| &m.tool_calls)
                .any(|c| c.id == "discarded")
        );
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn recovery_is_bounded_and_can_be_disabled() {
    for (retries, requests) in [(0, 1), (2, 3)] {
        let workspace = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new((0..4).map(|_| truncated()).collect()));
        let engine = WorkerEngine::builder(provider.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                profile(),
                workspace.path(),
                HostPolicy::default(),
                Value::Null,
            )
            .await
            .unwrap();
        let run = session
            .submit(
                "Create a file",
                "start",
                Limits {
                    max_retries: retries,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.error.unwrap().code, "model_output_limit");
        assert_eq!(provider.requests().len(), requests);
        assert!(run.status().await.unwrap().invocations.is_empty());
        assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 0);
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn recovery_obeys_request_and_step_budgets() {
    for limits in [
        Limits {
            max_model_requests: 1,
            ..Default::default()
        },
        Limits {
            max_steps: 1,
            ..Default::default()
        },
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            truncated(),
            ScriptedTurn::text("unused"),
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                profile(),
                workspace.path(),
                HostPolicy::default(),
                Value::Null,
            )
            .await
            .unwrap();
        let run = session
            .submit("Create a file", "start", limits, None)
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert!(matches!(
            result.error.unwrap().code.as_str(),
            "request_budget" | "step_budget"
        ));
        assert_eq!(provider.requests().len(), 1);
        engine.close().await.unwrap();
    }
}
