use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{
    context::{ContextBuilder, DefaultContextBuilder, validate_messages},
    policy::*,
    profiles::AgentProfile,
    providers::*,
    *,
};

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
fn read(path: &str) -> ScriptedTurn {
    call("files.read", json!({"path":path}))
}

#[tokio::test]
async fn varied_ranges_over_the_same_bytes_are_bounded_but_new_pages_are_allowed() {
    for new_pages in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("game.txt"),
            "one\ntwo\nthree\nfour\nfive\nsix\n",
        )
        .unwrap();
        let turns = if new_pages {
            (1..=6)
                .map(|line| {
                    call(
                        "files.read",
                        json!({"path":"game.txt","start_line":line,"end_line":line}),
                    )
                })
                .collect()
        } else {
            vec![
                call("files.read", json!({"path":"game.txt","start_byte":0})),
                call(
                    "files.read",
                    json!({"path":"./game.txt","start_line":1,"end_line":3}),
                ),
                call(
                    "files.read",
                    json!({"path":"/workspace/game.txt","start_line":2,"end_line":4}),
                ),
                call(
                    "files.read",
                    json!({"path":"game.txt","start_line":3,"end_line":5}),
                ),
                call(
                    "files.read",
                    json!({"path":"game.txt","start_line":4,"end_line":6}),
                ),
                call("files.read", json!({"path":"game.txt","start_byte":4})),
            ]
        };
        let (engine, session, provider) = fixture(turns, dir.path(), false).await;
        let run = session
            .submit(
                "Inspect needed ranges",
                "start",
                Limits {
                    max_steps: 6,
                    ..Limits::default()
                },
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(
            result.error.unwrap().code,
            if new_pages {
                "step_budget"
            } else {
                "repetition_limit"
            }
        );
        let saved: SessionData = serde_json::from_slice(
            &serde_json::to_vec(&session.snapshot().await.unwrap()).unwrap(),
        )
        .unwrap();
        let notices = saved.events.iter().filter(|e| matches!(&e.event, EventKind::Diagnostic { code } if code.starts_with("read_recovery:"))).count();
        assert_eq!(notices, if new_pages { 0 } else { 2 });
        if !new_pages {
            let r = saved.run(&run.id).unwrap();
            let (request, _) = DefaultContextBuilder
                .build(&saved, r, vec![])
                .await
                .unwrap();
            assert!(
                request
                    .messages
                    .iter()
                    .any(|m| m.content.contains("reads_without_new_information"))
            );
            assert!(
                provider.requests()[4]
                    .messages
                    .iter()
                    .any(|m| m.content.contains("reads_without_new_information"))
            );
            // The last fresh read still supplies exact content; recovery never
            // asks the model to edit from a hash alone after compaction.
            assert!(
                request
                    .messages
                    .iter()
                    .any(|m| m.role == Role::Tool && m.content.contains("two\\nthree"))
            );
            validate_messages(&request.messages).unwrap();
        }
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn identical_read_can_discover_an_external_change_before_repetition_guard() {
    struct ExternalChange {
        inner: Arc<ScriptedProvider>,
        path: std::path::PathBuf,
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ModelProvider for ExternalChange {
        fn identity(&self) -> String {
            "external-change-fixture".into()
        }
        fn capabilities(&self) -> ModelCapabilities {
            self.inner.capabilities()
        }
        async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                std::fs::write(&self.path, "externally updated").unwrap();
            }
            self.inner.stream(request).await
        }
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "original").unwrap();
    let inner = Arc::new(ScriptedProvider::new(vec![
        read("game.txt"),
        read("game.txt"),
    ]));
    let engine = WorkerEngine::builder(Arc::new(ExternalChange {
        inner,
        path: dir.path().join("game.txt"),
        calls: Default::default(),
    }))
    .build()
    .await
    .unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "coding-basic".into(),
                prompt: "Inspect".into(),
                tools: vec!["files.read".into(), "files.edit".into()],
                skills: vec![],
            },
            dir.path(),
            HostPolicy {
                workspace_writable: true,
                tool_permissions: [
                    ("files.read".into(), ToolPermission::Allow),
                    ("files.edit".into(), ToolPermission::Allow),
                ]
                .into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Inspect",
            "start",
            Limits {
                max_steps: 2,
                max_repeated_calls: 1,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
    let state = session.snapshot().await.unwrap();
    let reads = &state.run(&run.id).unwrap().invocations;
    assert_eq!(
        reads[1].result.as_ref().unwrap().data["content"],
        "externally updated"
    );
    assert_ne!(
        reads[0].result.as_ref().unwrap().data["sha256"],
        reads[1].result.as_ref().unwrap().data["sha256"]
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn rejected_large_rewrite_and_escalated_output_do_not_poison_repair_context() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "working implementation").unwrap();
    let proposal = "rejected new implementation\n".repeat(800);
    let (engine, session, _) = fixture(
        vec![call(
            "files.write",
            json!({"path":"game.txt","content":proposal}),
        )],
        dir.path(),
        false,
    )
    .await;
    let run = session
        .submit(
            "Preserve the existing implementation and verify it",
            "start",
            Limits {
                max_steps: 1,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
    let mut saved = session.snapshot().await.unwrap();
    let r = saved.run_mut(&run.id).unwrap();
    r.limits.context_tokens = 16384;
    r.limits.reserve_output_tokens = 15871;
    r.generation_recovery.initial_output_tokens = Some(4096);
    r.generation_recovery.target_output_tokens = Some(15871);
    let canonical = serde_json::to_value(&saved).unwrap();
    let (request, snapshot) = DefaultContextBuilder
        .build(&saved, saved.run(&run.id).unwrap(), vec![])
        .await
        .unwrap();
    assert!(request.max_output_tokens < 15871);
    assert!(snapshot.estimated_tokens + request.max_output_tokens + 512 <= 16384);
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.content == "Preserve the existing implementation and verify it")
    );
    assert!(
        request
            .messages
            .iter()
            .flat_map(|m| &m.tool_calls)
            .any(|c| c.arguments["content"]
                .as_str()
                .is_some_and(|s| s.contains("never applied")))
    );
    assert_eq!(serde_json::to_value(&saved).unwrap(), canonical);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "working implementation"
    );
    validate_messages(&request.messages).unwrap();
    engine.close().await.unwrap();
}

#[tokio::test]
async fn compaction_restores_a_working_anchor_from_persisted_receipts_once() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "exact anchor αβγ").unwrap();
    let (engine, session, _) = fixture(
        vec![
            read("game.txt"),
            read("game.txt"),
            call("files.list", json!({"path":"."})),
        ],
        dir.path(),
        false,
    )
    .await;
    let run = session
        .submit(
            "Repair the existing function",
            "start",
            Limits {
                max_steps: 3,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
    let mut saved: SessionData =
        serde_json::from_slice(&serde_json::to_vec(&session.snapshot().await.unwrap()).unwrap())
            .unwrap();
    let (before, _) = DefaultContextBuilder
        .build(&saved, saved.run(&run.id).unwrap(), vec![])
        .await
        .unwrap();
    assert!(
        !before
            .messages
            .iter()
            .any(|m| m.content.contains("restored_read_page"))
    );
    let indices = saved.run(&run.id).unwrap().invocations[..2]
        .iter()
        .flat_map(|i| [i.message_index, i.message_index + 1])
        .collect();
    saved.compactions.push(CompactionRecord {
        id: id("compact"),
        run_id: run.id.clone(),
        attempt_id: id("attempt"),
        message_indices: indices,
        source_hash: "fixture".into(),
        summary: "Earlier reads omitted".into(),
    });
    let (after, _) = DefaultContextBuilder
        .build(&saved, saved.run(&run.id).unwrap(), vec![])
        .await
        .unwrap();
    let pages: Vec<_> = after
        .messages
        .iter()
        .filter(|m| m.content.contains("restored_read_page"))
        .collect();
    assert_eq!(pages.len(), 1);
    assert!(pages[0].content.contains("exact anchor αβγ"));
    assert!(pages[0].content.contains(&hash("exact anchor αβγ")));
    assert_eq!(pages[0].role, Role::Assistant);
    validate_messages(&after.messages).unwrap();
    engine.close().await.unwrap();
}

#[tokio::test]
async fn read_recovery_feedback_cannot_be_echoed_as_task_completion() {
    struct Echo {
        inner: ScriptedProvider,
    }
    #[async_trait::async_trait]
    impl ModelProvider for Echo {
        fn identity(&self) -> String {
            "read-recovery-echo".into()
        }
        fn capabilities(&self) -> ModelCapabilities {
            self.inner.capabilities()
        }
        async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
            if let Some(message) = request
                .messages
                .iter()
                .find(|m| m.content.contains("\"event\":\"read_progress\""))
            {
                return Ok(Box::pin(futures_util::stream::iter(
                    ScriptedTurn::text(&message.content)
                        .deltas
                        .into_iter()
                        .map(Ok),
                )));
            }
            self.inner.stream(request).await
        }
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "unchanged").unwrap();
    let engine = WorkerEngine::builder(Arc::new(Echo {
        inner: ScriptedProvider::new(vec![read("game.txt"), read("game.txt")]),
    }))
    .build()
    .await
    .unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "coding-basic".into(),
                prompt: "Implement".into(),
                tools: vec!["files.read".into(), "files.edit".into()],
                skills: vec![],
            },
            dir.path(),
            HostPolicy {
                workspace_writable: true,
                tool_permissions: [("files.read".into(), ToolPermission::Allow)].into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Fix the game",
            "start",
            Limits {
                max_retries: 0,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Failed);
    assert_eq!(
        session
            .snapshot()
            .await
            .unwrap()
            .run(&run.id)
            .unwrap()
            .attempts
            .last()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .code,
        "model_feedback_echo"
    );
    engine.close().await.unwrap();
}
fn edit(before: &str, after: &str) -> ScriptedTurn {
    call(
        "files.edit",
        json!({"path":"game.txt","expected_hash":hash(before),"old":before,"new":after}),
    )
}
async fn fixture(
    turns: Vec<ScriptedTurn>,
    dir: &std::path::Path,
    denied: bool,
) -> (WorkerEngine, SessionHandle, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider::new(turns));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let names = [
        "files.read",
        "files.list",
        "files.write",
        "files.edit",
        "process.run",
    ];
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "coding-basic".into(),
                prompt: "Implement and verify".into(),
                tools: names.map(str::to_owned).to_vec(),
                skills: vec![],
            },
            dir,
            HostPolicy {
                workspace_writable: true,
                tool_permissions: names.map(|n| (n.into(), ToolPermission::Allow)).into(),
                hard_denied_tools: if denied {
                    vec!["files.read".into()]
                } else {
                    vec![]
                },
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    (engine, session, provider)
}

#[tokio::test]
async fn fresh_repeated_reads_restore_content_then_recover_to_verified_completion() {
    if let Err(error) = runtime::Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (engine,session,provider) = fixture(vec![
        call("files.write",json!({"path":"game.txt","content":"one"})),
        read("game.txt"), read("game.txt"),
        call("files.write",json!({"path":"game.txt","content":"overwrite"})),
        edit("one","two"), read("game.txt"), read("game.txt"), edit("two","three"),
        call("process.run",json!({"argv":["/bin/sh","-c","test \"$(cat game.txt)\" = three"],"verification":{"criteria":["Final fixture content is three"],"paths":["game.txt"]}})),
        ScriptedTurn::text("Verified final content"),
    ],dir.path(),false).await;
    let run = session
        .submit(
            "Implement",
            "start",
            Limits {
                max_repeated_calls: 1,
                ..Limits::continuous()
            },
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "three"
    );
    let data = session.snapshot().await.unwrap();
    let invocations = &data.run(&run.id).unwrap().invocations;
    let recoveries: Vec<_> = data
        .events
        .iter()
        .filter_map(|e| match &e.event {
            EventKind::Diagnostic { code } if code.starts_with("read_recovery:") => Some(code),
            _ => None,
        })
        .collect();
    assert_eq!(recoveries.len(), 2);
    assert!(
        recoveries
            .iter()
            .all(|code| code.as_str() == "read_recovery:1")
    );
    assert_eq!(
        invocations[2].result.as_ref().unwrap().data["content"],
        "one"
    );
    assert_eq!(
        invocations[6].result.as_ref().unwrap().data["content"],
        "two"
    );
    assert!(
        provider.requests()[3]
            .messages
            .iter()
            .any(|m| m.content.contains("reads_without_new_information"))
    );
    let conflict = invocations[3].result.as_ref().unwrap();
    assert_eq!(conflict.error_code.as_deref(), Some("write_conflict"));
    assert!(
        conflict.data["message"]
            .as_str()
            .unwrap()
            .contains("files.edit")
    );
    assert!(
        !conflict.data["message"]
            .as_str()
            .unwrap()
            .contains("supply expected_hash")
    );
    assert!(invocations[8].result.as_ref().unwrap().data["verification"]["passed"] == true);
    for request in provider.requests() {
        validate_messages(&request.messages).unwrap();
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn alternating_reads_and_failed_writes_cannot_replenish_recovery_or_bypass_denials() {
    for denied in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "a").unwrap();
        std::fs::write(dir.path().join("b"), "b").unwrap();
        let turns = if denied {
            vec![read("a"), read("a")]
        } else {
            vec![
                read("a"),
                read("b"),
                read("a"),
                read("a"),
                call(
                    "files.write",
                    json!({"path":"a","content":"failed overwrite"}),
                ),
                read("b"),
                read("a"),
            ]
        };
        let (engine, session, _) = fixture(turns, dir.path(), denied).await;
        let run = session
            .submit(
                "Inspect and implement",
                "start",
                Limits {
                    max_repeated_calls: 1,
                    ..Limits::continuous()
                },
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.error.unwrap().code, "repetition_limit");
        let saved: SessionData = serde_json::from_slice(
            &serde_json::to_vec(&session.snapshot().await.unwrap()).unwrap(),
        )
        .unwrap();
        let recoveries: Vec<_> = saved
            .events
            .iter()
            .filter_map(|e| match &e.event {
                EventKind::Diagnostic { code } if code.starts_with("read_recovery:") => Some(code),
                _ => None,
            })
            .collect();
        assert_eq!(recoveries.len(), if denied { 0 } else { 2 });
        if !denied {
            assert_eq!(recoveries[1], "read_recovery:2");
        }
        assert_eq!(std::fs::read_to_string(dir.path().join("a")).unwrap(), "a");
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn compacted_coding_context_ends_with_observed_files_instead_of_stale_empty_listing() {
    let dir = tempfile::tempdir().unwrap();
    let html = format!("{}\n</html>", "<!-- fixture -->\n".repeat(170));
    let js = format!(
        "{}\n// NEXT_SECTION",
        "// existing implementation\n".repeat(120)
    );
    let (engine, session, _) = fixture(
        vec![
            call("files.list", json!({"path":"."})),
            call("files.write", json!({"path":"index.html","content":html})),
            call("files.write", json!({"path":"game.js","content":js})),
            read("index.html"),
            read("game.js"),
        ],
        dir.path(),
        false,
    )
    .await;
    let run = session
        .submit(
            "Keep the existing interface",
            "start",
            Limits {
                max_steps: 5,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
    let mut data = session.snapshot().await.unwrap();
    let index = data.run(&run.id).unwrap().invocations[0].message_index;
    data.run_mut(&run.id).unwrap().limits.context_tokens = 14000;
    data.run_mut(&run.id).unwrap().limits.reserve_output_tokens = 4096;
    let (request, snapshot) = DefaultContextBuilder
        .build(&data, data.run(&run.id).unwrap(), vec![])
        .await
        .unwrap();
    validate_messages(&request.messages).unwrap();
    assert!(
        snapshot.omitted_messages.contains(&index),
        "old empty listing must be compactable: {:?}",
        snapshot.omitted_messages
    );
    let position = request
        .messages
        .iter()
        .rposition(|m| m.content.contains("\"event\":\"coding_task_state\""))
        .unwrap();
    assert!(
        request.messages[position + 1..]
            .iter()
            .all(|m| m.role != Role::Tool)
    );
    assert_eq!(request.messages[position].role, Role::Assistant);
    let evidence: Value = serde_json::from_str(&request.messages[position].content).unwrap();
    let files = evidence["data"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    let game = files.iter().find(|f| f["path"] == "game.js").unwrap();
    assert_eq!(game["sha256"], hash(&js));
    assert!(
        game["observed_tail_excerpt"]
            .as_str()
            .unwrap()
            .ends_with("// NEXT_SECTION")
    );
    assert!(request.messages[0].content.find("NEXT_SECTION").is_none());
    engine.close().await.unwrap();
}
