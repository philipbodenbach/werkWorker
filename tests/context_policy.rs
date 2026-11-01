#[path = "support/prefill.rs"]
mod prefill;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{policy::*, profiles::AgentProfile, providers::*, runtime::ArtifactStore, *};

fn profile(tools: &[&str]) -> AgentProfile {
    AgentProfile {
        schema_version: 1,
        name: "fixture".into(),
        prompt: "Keep hard constraints".into(),
        tools: tools.iter().map(|s| s.to_string()).collect(),
        skills: vec![],
    }
}
fn read(path: &str) -> ScriptedTurn {
    ScriptedTurn::calls(
        "",
        vec![ToolCall {
            id: id("call"),
            name: "files.read".into(),
            arguments: json!({"path":path}),
        }],
    )
}
fn policy(similar: bool) -> HostPolicy {
    HostPolicy {
        allow_similar: similar,
        grants: vec![CapabilityGrant {
            tool: "files.read".into(),
            version: "1".into(),
            resource: ".".into(),
            arguments_hash: None,
            expires_at_ms: None,
            preapproved: false,
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn model_compaction_is_budgeted_preserves_user_constraints_and_raw_history() {
    for mode in [harness::HarnessMode::Classic, harness::HarnessMode::Minimal] {
        let dir = tempfile::tempdir().unwrap();
        let long = "Historical evidence. ".repeat(100);
        let provider = Arc::new(prefill::PrefillProvider::new(vec![
            ScriptedTurn::text(&long),
            ScriptedTurn::text("Earlier evidence summary."),
            ScriptedTurn::text("Final answer."),
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .harness(harness::HarnessConfig {
                mode,
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(&[]), dir.path(), HostPolicy::default(), Value::Null)
            .await
            .unwrap();
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let picture = attachments::ImageAttachment::from_bytes(encoded.get_ref()).unwrap();
        let first = session
            .submit_with_images(
                "Never change customer identifiers",
                vec![picture.clone()],
                "first",
                Limits::default(),
                None,
            )
            .await
            .unwrap();
        first.wait().await.unwrap();
        let limits = Limits {
            model_compaction: true,
            ..Default::default()
        };
        let second = session
            .submit("Summarize next", "second", limits, None)
            .await
            .unwrap();
        assert_eq!(second.wait().await.unwrap().status, RunStatus::Completed);
        let state = session.snapshot().await.unwrap();
        assert_eq!(state.compactions.len(), 1);
        assert_eq!(state.compactions[0].message_indices, vec![1]);
        assert!(state.messages.iter().any(|m| m.content == long));
        assert_eq!(second.status().await.unwrap().metrics.model_requests, 2);
        let requests = provider.requests();
        assert!(
            requests[2]
                .messages
                .iter()
                .any(|m| m.content == "Never change customer identifiers")
        );
        assert!(!requests[2].messages.iter().any(|m| m.content == long));
        assert_eq!(
            requests[2]
                .messages
                .iter()
                .find(|m| m.role == Role::User)
                .unwrap()
                .images,
            vec![picture.clone()]
        );
        assert_eq!(state.messages[0].images, vec![picture]);
        if harness::config(&state, &state.runs[1]).unwrap().mode == harness::HarnessMode::Classic {
            assert!(
                requests[2].messages[0]
                    .content
                    .contains("Earlier evidence summary")
            );
        } else {
            assert_eq!(requests[1].messages.last().unwrap().role, Role::User);
            assert!(!requests[1].messages.last().unwrap().content.contains(&long));
            assert!(
                !requests[2].messages[0]
                    .content
                    .contains("Earlier evidence summary")
            );
            assert!(requests[2].messages.iter().any(
                |m| m.role == Role::Assistant && m.content.contains("Earlier evidence summary")
            ));
        }
        engine.close().await.unwrap();
    }
}
#[tokio::test]
async fn same_scope_approval_is_exact_explicit_and_does_not_cross_session() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), "alpha").unwrap();
    std::fs::write(dir.path().join("b"), "beta").unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        read("a"),
        read("a"),
        read("b"),
        ScriptedTurn::text("done"),
    ]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let session = engine
        .create_session(
            profile(&["files.read"]),
            dir.path(),
            policy(true),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("read", "c", Limits::default(), None)
        .await
        .unwrap();
    let mut events = run.events(0);
    let mut approvals = vec![];
    while let Some(e) = events.next().await {
        if let EventKind::ApprovalRequested { request } = e.unwrap().event {
            approvals.push(request.description["arguments"]["path"].clone());
            run.respond_to_approval(
                &request.request_id,
                ApprovalDecision::AllowSameArguments,
                &id("approve"),
            )
            .await
            .unwrap();
        }
    }
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(approvals, vec![json!("a"), json!("b")]);
    let another = engine
        .create_session(
            profile(&["files.read"]),
            dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    assert!(another.snapshot().await.unwrap().policy.grants.is_empty());
    engine.close().await.unwrap();
}
#[tokio::test]
async fn revocation_and_cross_run_approval_cannot_start_action() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), "alpha").unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![read("a")]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let session = engine
        .create_session(
            profile(&["files.read"]),
            dir.path(),
            policy(false),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("read", "c", Limits::default(), None)
        .await
        .unwrap();
    let mut events = run.events(0);
    let request = loop {
        if let EventKind::ApprovalRequested { request } =
            events.next().await.unwrap().unwrap().event
        {
            break request;
        }
    };
    session.revoke_tool("files.read", "revoke").await.unwrap();
    run.respond_to_approval(&request.request_id, ApprovalDecision::AllowOnce, "allow")
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().code, "grant_revoked");
    assert!(
        !session
            .snapshot()
            .await
            .unwrap()
            .events
            .iter()
            .any(|e| matches!(e.event, EventKind::ToolStarted { .. }))
    );
    engine.close().await.unwrap();
}
#[tokio::test]
async fn separate_sessions_run_independently_including_shared_workspace() {
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn {
            delay_ms: 150,
            ..ScriptedTurn::text("one")
        },
        ScriptedTurn {
            delay_ms: 150,
            ..ScriptedTurn::text("two")
        },
        ScriptedTurn::text("three"),
    ]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let first = engine
        .create_session(
            profile(&[]),
            first_dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let second = engine
        .create_session(
            profile(&[]),
            second_dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let shared = engine
        .create_session(
            profile(&[]),
            first_dir.path(),
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap();
    let run = first
        .submit("first", "c", Limits::default(), None)
        .await
        .unwrap();
    let shared_run = shared
        .submit("shared", "x", Limits::default(), None)
        .await
        .unwrap();
    let other = second
        .submit("second", "c", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(other.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(
        shared_run.wait().await.unwrap().status,
        RunStatus::Completed
    );
    assert!(
        !second
            .snapshot()
            .await
            .unwrap()
            .messages
            .iter()
            .any(|m| m.content == "first")
    );
    engine.close().await.unwrap();
}
#[tokio::test]
async fn large_output_has_owned_artifact_and_context_excerpt() {
    let dir = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("large"), "a".repeat(20_000)).unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        read("large"),
        ScriptedTurn::text("done"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .artifacts(ArtifactStore::open(artifacts.path()).await.unwrap())
        .build()
        .await
        .unwrap();
    let mut policy = policy(false);
    policy.grants[0].preapproved = true;
    let session = engine
        .create_session(profile(&["files.read"]), dir.path(), policy, Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("read", "c", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let state = run.status().await.unwrap();
    let result = state.invocations[0].result.as_ref().unwrap();
    assert!(result.truncated);
    assert_eq!(result.artifacts.len(), 1);
    assert_eq!(result.data["path"], "large");
    assert_eq!(result.data["sha256"], hash("a".repeat(20_000)));
    assert!(result.data.to_string().len() < 9000);
    assert!(
        provider.requests()[1]
            .messages
            .iter()
            .any(|m| m.content.contains("\"event\":\"tool_result_truncated\""))
    );
    let data = session.snapshot().await.unwrap();
    let aid =
        &data.metadata["_werkworker_harness"][&run.id]["adaptive"]["aids"]["tool_result_truncated"];
    assert_eq!(aid["active"], false);
    assert_eq!(aid["scope"], "files.read:truncated");
    engine.close().await.unwrap();
}

struct BoundedProvider {
    inner: Arc<ScriptedProvider>,
    limit: usize,
}

#[tokio::test]
async fn large_file_reads_keep_complete_text_lines_and_a_precise_next_page() {
    let dir = tempfile::tempdir().unwrap();
    let lines: Vec<_> = (1..=400)
        .map(|n| format!("{n}: const text = \"Grüße 世界, no escaped JSON excerpt\";"))
        .collect();
    let source = lines.join("\n");
    std::fs::write(dir.path().join("large"), &source).unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        read("large"),
        ScriptedTurn::calls(
            "",
            vec![ToolCall {
                id: id("call"),
                name: "files.read".into(),
                arguments: json!({"path":"large","start_line":201,"end_line":210}),
            }],
        ),
        ScriptedTurn::text("done"),
    ]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let mut grants = policy(false);
    grants.grants[0].preapproved = true;
    let session = engine
        .create_session(profile(&["files.read"]), dir.path(), grants, Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("Read the file", "read", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let state = run.status().await.unwrap();
    let first = state.invocations[0].result.as_ref().unwrap();
    assert!(first.truncated);
    assert!(first.data.get("excerpt").is_none());
    assert_eq!(first.data["sha256"], hash(&source));
    assert_eq!(first.data["start_line"], 1);
    assert_eq!(first.data["total_lines"], 400);
    let end = first.data["end_line"].as_u64().unwrap() as usize;
    assert!(end > 1 && end < 400);
    assert_eq!(first.data["next_start_line"], end + 1);
    assert_eq!(first.data["content"], lines[..end].join("\n"));
    assert!(serde_json::to_vec(&first.data).unwrap().len() <= 8192);
    let next = state.invocations[1].result.as_ref().unwrap();
    assert!(!next.truncated);
    assert_eq!(next.data["start_line"], 201);
    assert_eq!(next.data["end_line"], 210);
    assert_eq!(next.data["content"], lines[200..210].join("\n"));
    assert_eq!(next.data["sha256"], first.data["sha256"]);
    engine.close().await.unwrap();
}

#[tokio::test]
async fn followups_compact_previous_final_code_and_keep_the_session_usable() {
    let dir = tempfile::tempdir().unwrap();
    let code = format!(
        "```rust\n{}\n```",
        "let value = 123; // previous implementation\n".repeat(240)
    );
    let scripted = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::text(&code),
        ScriptedTurn::text("Here is the requested adjustment."),
        ScriptedTurn::text("The conversation continues."),
    ]));
    let engine = WorkerEngine::builder(Arc::new(BoundedProvider {
        inner: scripted.clone(),
        limit: 8192,
    }))
    .build()
    .await
    .unwrap();
    let session = engine
        .create_session(profile(&[]), dir.path(), HostPolicy::default(), Value::Null)
        .await
        .unwrap();
    for (i, prompt) in [
        "Implement the algorithm; preserve all customer identifiers.",
        "Put it in a single main.rs.",
        "Can we continue?",
    ]
    .iter()
    .enumerate()
    {
        let run = session
            .submit(*prompt, &format!("turn-{i}"), Limits::default(), None)
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    }
    let data = session.snapshot().await.unwrap();
    assert_eq!(data.runs.len(), 3);
    assert!(data.messages.iter().any(|m| m.content == code));
    assert!(data.runs[1].contexts[0].omitted_messages.contains(&1));
    let requests = scripted.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert!(request.messages.iter().any(|m| {
            m.content == "Implement the algorithm; preserve all customer identifiers."
        }));
        assert!(!request.messages.iter().any(|m| m.content == code));
        werk_worker::context::validate_messages(&request.messages).unwrap();
    }
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|m| m.content == "Put it in a single main.rs.")
    );
    assert!(
        requests[0].messages[0]
            .content
            .contains("respect host policy")
    );
    engine.close().await.unwrap();
}
#[async_trait::async_trait]
impl ModelProvider for BoundedProvider {
    fn identity(&self) -> String {
        "bounded-fixture".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            context_tokens: Some(self.limit),
            ..self.inner.capabilities()
        }
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        assert!(
            werk_worker::context::estimate_request(&request) + request.max_output_tokens + 512
                <= self.limit
        );
        self.inner.stream(request).await
    }
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn compaction_retains_loaded_skill_and_directory_evidence() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("workspace_marker.txt"), "present").unwrap();
    std::fs::write(
        dir.path().join("evidence"),
        "Detailed evidence 世界. ".repeat(80),
    )
    .unwrap();
    let call = |name: &str, arguments| {
        ScriptedTurn::calls(
            "",
            vec![ToolCall {
                id: id("call"),
                name: name.into(),
                arguments,
            }],
        )
    };
    let mut turns = vec![
        call("skills.load", json!({"name":"coding-basic"})),
        call("files.list", json!({"path":"."})),
    ];
    turns.extend((0..8).map(|_| read("evidence")));
    turns.push(ScriptedTurn::text("done"));
    let scripted = Arc::new(ScriptedProvider::new(turns));
    let engine = WorkerEngine::builder(Arc::new(BoundedProvider {
        inner: scripted.clone(),
        limit: 12288,
    }))
    .build()
    .await
    .unwrap();
    let mut agent = profile(&["skills.load", "files.list", "files.read"]);
    agent.skills = vec!["coding-basic".into()];
    let mut granted = policy(false);
    granted.grants[0].preapproved = true;
    granted.grants.push(CapabilityGrant {
        tool: "files.list".into(),
        ..granted.grants[0].clone()
    });
    let session = engine
        .create_session(agent, dir.path(), granted, Value::Null)
        .await
        .unwrap();
    let run = session
        .submit(
            "Implement a file; preserve user changes",
            "start",
            Limits {
                max_repeated_calls: 20,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert!(
        run.status()
            .await
            .unwrap()
            .contexts
            .iter()
            .any(|c| !c.omitted_messages.is_empty())
    );
    let expected_skill = run
        .status()
        .await
        .unwrap()
        .invocations
        .iter()
        .find(|i| i.call.name == "skills.load")
        .unwrap()
        .result
        .as_ref()
        .unwrap()
        .data["body"]
        .clone();
    assert!(expected_skill.as_str().is_some_and(|body| !body.is_empty()));
    for request in &scripted.requests()[3..] {
        assert!(
            !request.messages[0]
                .content
                .contains("Skills already loaded successfully")
        );
        assert!(
            request
                .messages
                .iter()
                .any(|m| m.role != Role::System && m.content.contains("coding-basic"))
        );
        let results: Vec<ToolResult> = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| serde_json::from_str(&m.content).unwrap())
            .collect();
        assert!(
            results.iter().any(|r| r.data["body"] == expected_skill),
            "loaded skill lost"
        );
        assert!(
            results.iter().any(|r| r.data["entries"]
                .as_array()
                .is_some_and(|entries| entries.contains(&json!("workspace_marker.txt")))),
            "directory result lost"
        );
        werk_worker::context::validate_messages(&request.messages).unwrap();
    }
    engine.close().await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn automatic_compaction_keeps_long_tool_runs_inside_provider_context() {
    for mode in [harness::HarnessMode::Classic, harness::HarnessMode::Minimal] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("evidence"), "Evidence 世界. ".repeat(130)).unwrap();
        let mut turns = (0..10).map(|_| read("evidence")).collect::<Vec<_>>();
        turns.push(ScriptedTurn::text("done"));
        let scripted = Arc::new(ScriptedProvider::new(turns));
        let provider = Arc::new(BoundedProvider {
            inner: scripted.clone(),
            limit: 8192,
        });
        let engine = WorkerEngine::builder(provider)
            .harness(harness::HarnessConfig {
                mode,
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        let mut granted = policy(false);
        granted.grants[0].preapproved = true;
        let session = engine
            .create_session(profile(&["files.read"]), dir.path(), granted, Value::Null)
            .await
            .unwrap();
        let run = session
            .submit(
                "Never alter customer identifiers",
                "long",
                Limits {
                    max_repeated_calls: 20,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
        let state = run.status().await.unwrap();
        assert!(
            state
                .contexts
                .iter()
                .any(|c| !c.omitted_messages.is_empty() && c.summary.is_some())
        );
        assert_eq!(
            session
                .snapshot()
                .await
                .unwrap()
                .messages
                .iter()
                .filter(|m| m.role == Role::Tool)
                .count(),
            10
        );
        let data = session.snapshot().await.unwrap();
        let aid =
            &data.metadata["_werkworker_harness"][&run.id]["adaptive"]["aids"]["context_pressure"];
        assert_eq!(aid["active"], false);
        assert_eq!(aid["episodes"], 1);
        assert_eq!(
            scripted
                .requests()
                .iter()
                .filter(|r| r
                    .messages
                    .iter()
                    .any(|m| m.content.contains("\"event\":\"context_pressure\"")))
                .count(),
            1
        );
        for request in scripted.requests() {
            werk_worker::context::validate_messages(&request.messages).unwrap();
            assert!(
                request
                    .messages
                    .iter()
                    .any(|m| m.content == "Never alter customer identifiers")
            );
            assert!(request.max_output_tokens <= 2048);
        }
        engine.close().await.unwrap();
    }
}
#[tokio::test]
async fn oversized_constraints_fail_before_provider_and_output_reserve_is_clamped() {
    let dir = tempfile::tempdir().unwrap();
    let scripted = Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text("ok")]));
    let engine = WorkerEngine::builder(Arc::new(BoundedProvider {
        inner: scripted.clone(),
        limit: 4096,
    }))
    .build()
    .await
    .unwrap();
    let session = engine
        .create_session(profile(&[]), dir.path(), HostPolicy::default(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("hello", "small", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(scripted.requests()[0].max_output_tokens, 1024);
    let run = session
        .submit("約束".repeat(2000), "large", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(
        run.wait().await.unwrap().error.unwrap().code,
        "context_overflow"
    );
    assert_eq!(scripted.requests().len(), 1);
    engine.close().await.unwrap();
    assert!(
        Limits {
            reserve_output_tokens: usize::MAX,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
}

#[tokio::test]
async fn small_context_overflow_borrows_output_reserve_without_losing_constraints() {
    struct Counted {
        inner: Arc<ScriptedProvider>,
        input: usize,
    }
    #[async_trait::async_trait]
    impl ModelProvider for Counted {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                context_tokens: Some(16384),
                ..self.inner.capabilities()
            }
        }
        fn identity(&self) -> String {
            "counted".into()
        }
        async fn count_input_tokens(&self, _: &ModelRequest) -> Result<Option<usize>> {
            Ok(Some(self.input))
        }
        async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
            assert!(self.input + request.max_output_tokens + 512 <= 16384);
            self.inner.stream(request).await
        }
    }
    for (input, output) in [(11776, Some(4096)), (11866, Some(3942)), (13000, None)] {
        let scripted = Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text("done")]));
        let engine = WorkerEngine::builder(Arc::new(Counted {
            inner: scripted.clone(),
            input,
        }))
        .build()
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let session = engine
            .create_session(
                AgentProfile::bundled("coding-basic").unwrap(),
                dir.path(),
                HostPolicy {
                    workspace_writable: true,
                    request_approvals: true,
                    ..Default::default()
                },
                Value::Null,
            )
            .await
            .unwrap();
        let original = "Keep all requirements, including the reference image.";
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(677, 680)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let picture = attachments::ImageAttachment::from_bytes(encoded.get_ref()).unwrap();
        let run = session
            .submit_with_images(
                original,
                vec![picture.clone()],
                "start",
                Limits {
                    context_tokens: 16384,
                    reserve_output_tokens: 4096,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        if let Some(output) = output {
            assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
            let requests = scripted.requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].max_output_tokens, output);
            assert!(!requests[0].messages[0].content.contains("Output budget:"));
            assert_eq!(requests[0].messages[1].content, original);
            assert_eq!(requests[0].messages[1].images, vec![picture.clone()]);
        } else {
            assert_eq!(result.error.unwrap().code, "context_overflow");
            assert!(scripted.requests().is_empty());
        }
        assert_eq!(
            run.status().await.unwrap().limits.reserve_output_tokens,
            4096
        );
        let saved = session.snapshot().await.unwrap();
        assert_eq!(saved.messages[0].content, original);
        assert_eq!(saved.messages[0].images, vec![picture]);
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn followup_retains_prior_write_plan_and_read_page_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let content = "first line\nsecond line\n";
    let call = |name: &str, arguments: Value| {
        ScriptedTurn::calls(
            "",
            vec![ToolCall {
                id: id("call"),
                name: name.into(),
                arguments,
            }],
        )
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        call(
            "task.plan",
            json!({"steps":[{"id":"1","description":"Build then verify the existing game","status":"in_progress"}]}),
        ),
        call("files.write", json!({"path":"game.html","content":content})),
        call(
            "files.read",
            json!({"path":"game.html","start_line":1,"end_line":1}),
        ),
        call(
            "files.read",
            json!({"path":"game.html","start_line":2,"end_line":2}),
        ),
        ScriptedTurn::text("Implementation saved; verification remains."),
        ScriptedTurn::text("Continued from the observed file."),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(&["task.plan", "files.write", "files.read"]),
            dir.path(),
            HostPolicy {
                workspace_writable: true,
                request_approvals: true,
                tool_permissions: ["files.write", "files.read"]
                    .into_iter()
                    .map(|n| (n.into(), ToolPermission::Allow))
                    .collect(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let first = session
        .submit("Build the game", "first", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(first.wait().await.unwrap().status, RunStatus::Completed);
    let second = session
        .submit("Continue verification", "second", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(second.wait().await.unwrap().status, RunStatus::Completed);
    let requests = provider.requests();
    let prompt = requests
        .last()
        .unwrap()
        .messages
        .iter()
        .filter(|m| m.role != Role::System)
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(prompt.contains(&hash(content)));
    assert!(prompt.contains("game.html"));
    assert!(prompt.contains("Build then verify the existing game"));
    assert!(prompt.contains("\"start_line\":1"));
    assert!(prompt.contains("\"start_line\":2"));
    assert!(prompt.contains("\"total_lines\":2"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.html")).unwrap(),
        content
    );
    assert_eq!(
        session.snapshot().await.unwrap().runs[0].invocations.len(),
        4
    );
    engine.close().await.unwrap();
}
