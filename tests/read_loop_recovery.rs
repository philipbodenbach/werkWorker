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
async fn varied_covered_ranges_stop_after_recovery_but_new_pages_remain_available() {
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
                "read_no_progress"
            }
        );
        let saved: SessionData = serde_json::from_slice(
            &serde_json::to_vec(&session.snapshot().await.unwrap()).unwrap(),
        )
        .unwrap();
        let notices = saved.events.iter().filter(|e| matches!(&e.event, EventKind::Diagnostic { code } if code.starts_with("read_recovery:"))).count();
        assert_eq!(notices, if new_pages { 0 } else { 2 });
        assert!(
            provider.requests()[5]
                .tools
                .iter()
                .any(|t| t.name == "files.read")
        );
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
    let anchor = "exact anchor αβγ\n[\"A4\",1]\nC:\\notes\n```\n";
    std::fs::write(dir.path().join("game.txt"), anchor).unwrap();
    let (engine, session, _) = fixture(
        vec![
            call("files.read", json!({"path":"game.txt","start_byte":0})),
            call("files.read", json!({"path":"game.txt","start_byte":0})),
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
                max_repeated_calls: 1,
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
        .build(&saved, saved.run(&run.id).unwrap(), engine.tools())
        .await
        .unwrap();
    assert!(after.tools.iter().any(|t| t.name == "files.read"));
    assert!(after.tools.iter().any(|t| t.name == "files.edit"));
    let pages: Vec<_> = after
        .messages
        .iter()
        .filter(|m| m.content.contains("restored_read_page"))
        .collect();
    assert_eq!(pages.len(), 1);
    assert!(
        pages[0].content.contains(anchor),
        "anchors must be verbatim, not JSON-escaped"
    );
    assert!(pages[0].content.contains(&hash(anchor)));
    assert_eq!(pages[0].role, Role::Assistant);
    validate_messages(&after.messages).unwrap();
    let restored = restored_pages(&after).remove(0);
    assert_eq!(restored["content"], anchor);
    engine.close().await.unwrap();

    // Use the rendered evidence as an actual edit anchor, including its quote,
    // backslash, Unicode and fence characters; do not repair escapes by hand.
    let replacement = format!("{anchor}implemented\n");
    let (engine, session, _) = fixture(
        vec![call(
            "files.edit",
            json!({"path":"game.txt", "expected_hash":restored["sha256"],
            "old":restored["content"], "new":replacement}),
        )],
        dir.path(),
        false,
    )
    .await;
    let run = session
        .submit(
            "Apply the retained anchor",
            "edit",
            Limits {
                max_steps: 1,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        replacement
    );
    engine.close().await.unwrap();
}

fn restored_pages(request: &ModelRequest) -> Vec<Value> {
    request
        .messages
        .iter()
        .filter_map(|m| {
            let header = m.content.lines().next()?;
            let value: Value = serde_json::from_str(header).ok()?;
            (value["event"] == "restored_read_page").then(|| {
                assert_eq!(m.role, Role::Assistant);
                let mut data = value["data"].clone();
                if data["content_format"] == "verbatim_fence" {
                    let (_, framed) = m.content.split_once('\n').unwrap();
                    let (opening, body) = framed.split_once('\n').unwrap();
                    let fence = opening.strip_suffix("text").unwrap();
                    assert!(fence.len() >= 3 && fence.bytes().all(|b| b == b'`'));
                    let bytes = data["content_bytes"].as_u64().unwrap() as usize;
                    let content = &body[..bytes];
                    assert!(!content.contains(fence));
                    assert_eq!(&body[bytes..], format!("\n{fence}"));
                    data["content"] = json!(content);
                }
                data
            })
        })
        .collect()
}

struct ReasoningCounter(u64);

struct LimitedWorkingSetCounter {
    oversized_first: bool,
}
#[async_trait::async_trait]
impl ModelProvider for LimitedWorkingSetCounter {
    fn identity(&self) -> String {
        "limited-working-set-fixture".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn estimate_input_tokens(&self, request: &ModelRequest) -> Result<usize> {
        // Stable instructions/tools plus optional file evidence. Two pages
        // exceed the input allowance; one useful page fits at full output.
        Ok(10_000
            + restored_pages(request)
                .iter()
                .map(|page| {
                    if self.oversized_first
                        && page["start_byte"] == 0
                        && page["content"].as_str().unwrap().len() > 1024
                    {
                        3_000
                    } else {
                        1_000
                    }
                })
                .sum::<usize>())
    }
    async fn stream(&self, _: ModelRequest) -> Result<ModelStream> {
        panic!("working-set budget check never generates")
    }
}

#[async_trait::async_trait]
impl ModelProvider for ReasoningCounter {
    fn identity(&self) -> String {
        "working-context-reasoning-fixture".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            context_tokens: Some(16384),
            ..Default::default()
        }
    }
    fn max_output_tokens(&self) -> Option<usize> {
        Some(4096)
    }
    async fn reasoning_options(&self, output: usize) -> Result<Option<ReasoningOptions>> {
        Ok(Some(serde_json::from_value(json!({
            "model":"fixture", "requested":"default", "native_effort":null,
            "profile":{"schema_version":1,"model":"fixture","supported":true,
                "default_tier":"high","default_adjusted_to_limits":false,
                "limits":{"context_tokens":16384,"output_tokens":output,"reasoning_tokens":self.0,"answer_reserve_tokens":512},
                "tiers":[{"id":"high","label":"High","available":true,"reasoning_tokens":self.0,"minimum_output_tokens":self.0 + 512}]}
        }))?))
    }
    async fn estimate_input_tokens(&self, request: &ModelRequest) -> Result<usize> {
        Ok(serde_json::to_vec(request)?.len() / 4 + 256)
    }
    async fn stream(&self, _: ModelRequest) -> Result<ModelStream> {
        panic!("context budget check never generates")
    }
}

#[tokio::test]
async fn compaction_preserves_edited_code_when_latest_reads_target_another_file() {
    let dir = tempfile::tempdir().unwrap();
    let lines = [
        format!("const G = {{board: []}}; // {}\n", "state α ".repeat(90)),
        format!(
            "function step() {{ G.board.push(1); }} // {}\n",
            "step β ".repeat(90)
        ),
        format!(
            "function spawnDust() {{ return G.board.length; }} // {}\n",
            "tail γ ".repeat(90)
        ),
    ];
    let js = lines.concat();
    let html = "<canvas id=game></canvas><script src=game.js></script>\n";
    let mut turns = vec![
        call("files.write", json!({"path":"index.html","content":html})),
        call("files.write", json!({"path":"game.js","content":js})),
    ];
    for line in 1..=3 {
        turns.push(call(
            "files.read",
            json!({"path":"game.js","start_line":line,"end_line":line}),
        ));
    }
    // Exercise retention before another stall, independently of the mandatory
    // check phase tested in work_cycle.rs after six investigation calls.
    turns.push(read("index.html"));
    turns.push(call(
        "files.search",
        json!({"path":".","query":"placeholder__resume"}),
    ));
    let (engine, session, _) = fixture(turns, dir.path(), false).await;
    let run = session
        .submit(
            "Finish a runnable game, then verify it",
            "start",
            Limits {
                max_steps: 7,
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
    // All reads/writes have left the rolling transcript, while the search and
    // durable receipts survive. The most recent read concerns HTML, not JS.
    let indices = saved.run(&run.id).unwrap().invocations[..6]
        .iter()
        .flat_map(|i| [i.message_index, i.message_index + 1])
        .collect();
    saved.compactions.push(CompactionRecord {
        id: id("compact"),
        run_id: run.id.clone(),
        attempt_id: id("attempt"),
        message_indices: indices,
        source_hash: "fixture".into(),
        summary: "Earlier code omitted".into(),
    });
    let canonical = serde_json::to_value(&saved).unwrap();
    let tools = || {
        engine
            .tools()
            .into_iter()
            .filter(|t| saved.profile.tools.contains(&t.name))
            .collect()
    };
    let (request, snapshot) = DefaultContextBuilder
        .build(&saved, saved.run(&run.id).unwrap(), tools())
        .await
        .unwrap();
    assert!(request.tools.iter().any(|t| t.name == "files.read"));
    let pages = restored_pages(&request);
    assert_eq!(
        pages.len(),
        2,
        "restore current JS and HTML from successful writes, without duplicate read ranges"
    );
    for line in &lines {
        assert!(pages.iter().any(|p| {
            p["path"] == "game.js"
                && p["sha256"] == hash(&js)
                && p["content"]
                    .as_str()
                    .is_some_and(|s| s.contains(line.trim_end()))
        }));
    }
    assert!(pages.iter().any(|p| {
        p["path"] == "index.html"
            && p["content"]
                .as_str()
                .is_some_and(|s| s.trim_end() == html.trim_end())
    }));
    assert_eq!(serde_json::to_value(&saved).unwrap(), canonical);
    validate_messages(&request.messages).unwrap();

    // Optional restoration must not make an otherwise admissible request fail.
    let mut tight = saved.clone();
    tight.run_mut(&run.id).unwrap().limits.context_tokens =
        snapshot.estimated_tokens + request.max_output_tokens + 512 - js.len();
    let (request, snapshot) = DefaultContextBuilder
        .build(&tight, tight.run(&run.id).unwrap(), tools())
        .await
        .unwrap();
    assert!(
        snapshot.estimated_tokens + request.max_output_tokens + 512
            <= tight.run(&run.id).unwrap().limits.context_tokens
    );
    assert!(request.tools.iter().any(|t| t.name == "files.read"));
    validate_messages(&request.messages).unwrap();

    // Under a tight native/provider budget, preserve the highest-priority
    // page that fits. Dropping every restored page recreates the read loop.
    let mut constrained = saved.clone();
    constrained.run_mut(&run.id).unwrap().limits.context_tokens = 16_384;
    constrained
        .run_mut(&run.id)
        .unwrap()
        .limits
        .reserve_output_tokens = 4_096;
    for oversized_first in [false, true] {
        let (request, snapshot) = DefaultContextBuilder
            .build_with_provider(
                &constrained,
                constrained.run(&run.id).unwrap(),
                tools(),
                &LimitedWorkingSetCounter { oversized_first },
            )
            .await
            .unwrap();
        let retained = restored_pages(&request);
        assert_eq!(retained.len(), 1, "retain what fits instead of all-or-none");
        assert_eq!(
            retained[0]["path"],
            if oversized_first {
                "index.html"
            } else {
                "game.js"
            }
        );
        assert_eq!(
            retained[0]["sha256"],
            hash(if oversized_first { html } else { &js })
        );
        assert_eq!(request.max_output_tokens, 4_096);
        assert!(snapshot.estimated_tokens + request.max_output_tokens + 512 <= 16_384);
        validate_messages(&request.messages).unwrap();
    }

    // Restored code competes with input, while thinking and tool JSON share
    // the reserved output. High effort must reduce the advertised edit size,
    // not silently spend the answer allowance a second time.
    let mut thinking = saved.clone();
    thinking.metadata["_werkworker_reasoning"] =
        json!({"provider":"working-context-reasoning-fixture","tier":"high"});
    thinking.run_mut(&run.id).unwrap().limits.context_tokens = 16384;
    thinking
        .run_mut(&run.id)
        .unwrap()
        .limits
        .reserve_output_tokens = 4096;
    let mut previous_target = u64::MAX;
    for reasoning in [0, 2048, 3072] {
        let (request, snapshot) = DefaultContextBuilder
            .build_with_provider(
                &thinking,
                thinking.run(&run.id).unwrap(),
                tools(),
                &ReasoningCounter(reasoning),
            )
            .await
            .unwrap();
        assert_eq!(request.reasoning_tier.as_deref(), Some("high"));
        assert_eq!(restored_pages(&request).len(), 2);
        assert!(snapshot.estimated_tokens + request.max_output_tokens + 512 <= 16384);
        let edit = request
            .tools
            .iter()
            .find(|t| t.name == "files.edit")
            .unwrap();
        let budget = &edit.input_schema["x-werk-file-io"];
        assert_eq!(budget["reasoning_tokens"], reasoning);
        assert_eq!(budget["output_tokens"], request.max_output_tokens);
        let target = budget["target_new_characters"].as_u64().unwrap();
        assert!(target < previous_target);
        if reasoning == 2048 {
            assert_eq!(target, 3328);
        }
        previous_target = target;
    }

    // Later observations invalidate historical code regardless of whether the
    // change came from a file edit, shell command, or a missing-file result.
    for kind in ["edit", "process", "missing"] {
        let mut changed = saved.clone();
        let current = changed.run_mut(&run.id).unwrap();
        let mut change = current.invocations.last().unwrap().clone();
        change.invocation_id = id("change");
        change.call.id = id("call");
        change.call.arguments = json!({"path":"game.js"});
        change.call.name = match kind {
            "edit" => "files.edit",
            "process" => "process.run",
            _ => "files.read",
        }
        .into();
        change.result = Some(match kind {
            "edit" => ToolResult::ok(json!({"path":"game.js","sha256":hash("updated code")})),
            "process" => ToolResult::ok(
                json!({"workspace_changes":{"game.js":{"status":"present","sha256":hash("updated code")}}}),
            ),
            _ => ToolResult::error(Outcome::Failed, "not_found"),
        });
        current.invocations.push(change);
        let (request, _) = DefaultContextBuilder
            .build(&changed, changed.run(&run.id).unwrap(), tools())
            .await
            .unwrap();
        assert!(
            restored_pages(&request)
                .iter()
                .all(|p| p["path"] != "game.js"),
            "{kind} must invalidate stale code"
        );
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn successful_edit_retains_current_head_and_tail_without_rereading() {
    let dir = tempfile::tempdir().unwrap();
    let before = format!(
        "header before α\n{}middle contract Δ\n{}tail anchor Ω\n",
        "body αβγ\n".repeat(1000),
        "body αβγ\n".repeat(1000)
    );
    let after = before.replacen("header before α", "header after β", 1);
    std::fs::write(dir.path().join("state.txt"), &before).unwrap();
    let turns = vec![
        call(
            "files.read",
            json!({"path":"state.txt","start_line":1,"end_line":1}),
        ),
        call(
            "files.read",
            json!({"path":"state.txt","start_line":900,"end_line":1800}),
        ),
        call("files.read", json!({"path":"state.txt","start_line":2003})),
        call(
            "files.edit",
            json!({"path":"state.txt","expected_hash":hash(&before),"old":"header before α","new":"header after β"}),
        ),
    ];
    let (engine, session, _) = fixture(turns, dir.path(), false).await;
    let run = session
        .submit(
            "Update the header while preserving the rest",
            "start",
            Limits {
                max_steps: 4,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("state.txt")).unwrap(),
        after
    );
    let mut saved = session.snapshot().await.unwrap();
    let indices = saved
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m.role, Role::Assistant | Role::Tool))
        .map(|(i, _)| i)
        .collect();
    saved.compactions.push(CompactionRecord {
        id: id("compaction"),
        run_id: run.id.clone(),
        attempt_id: id("attempt"),
        message_indices: indices,
        source_hash: "fixture".into(),
        summary: "Previous reads omitted".into(),
    });
    let canonical = serde_json::to_value(&saved).unwrap();
    let tools = || {
        engine
            .tools()
            .into_iter()
            .filter(|t| saved.profile.tools.contains(&t.name))
            .collect()
    };
    let (request, _) = DefaultContextBuilder
        .build(&saved, saved.run(&run.id).unwrap(), tools())
        .await
        .unwrap();
    let pages = restored_pages(&request);
    assert_eq!(pages.len(), 3);
    assert!(
        pages
            .iter()
            .any(|p| p["content"].as_str().unwrap().contains("middle contract Δ"))
    );
    assert!(
        pages
            .iter()
            .all(|p| p["sha256"] == hash(&after) && p["source_kind"] == "successful_file_change")
    );
    assert!(
        pages
            .iter()
            .any(|p| p["content"].as_str().unwrap().contains("header after β"))
    );
    assert!(
        pages
            .iter()
            .any(|p| p["content"].as_str().unwrap().contains("tail anchor Ω"))
    );
    assert!(
        !pages
            .iter()
            .any(|p| p["content"].as_str().unwrap().contains("header before α"))
    );
    assert!(
        pages
            .iter()
            .map(|p| p["content"].as_str().unwrap().len())
            .sum::<usize>()
            <= 12_288
    );
    validate_messages(&request.messages).unwrap();
    assert_eq!(serde_json::to_value(&saved).unwrap(), canonical);
    let mut constrained = saved.clone();
    constrained.run_mut(&run.id).unwrap().limits.context_tokens = 16_384;
    constrained
        .run_mut(&run.id)
        .unwrap()
        .limits
        .reserve_output_tokens = 4_096;
    let (request, snapshot) = DefaultContextBuilder
        .build_with_provider(
            &constrained,
            constrained.run(&run.id).unwrap(),
            tools(),
            &LimitedWorkingSetCounter {
                oversized_first: true,
            },
        )
        .await
        .unwrap();
    let retained = restored_pages(&request);
    assert_eq!(retained.len(), 1);
    assert!(
        retained[0]["content"]
            .as_str()
            .unwrap()
            .contains("tail anchor Ω")
    );
    assert_eq!(retained[0]["sha256"], hash(&after));
    assert!(snapshot.estimated_tokens + request.max_output_tokens + 512 <= 16_384);
    // Prepared proposals only count when their content matches a successful
    // current-version receipt; stale or corrupted data cannot be restored.
    let mut invalid = saved.clone();
    invalid
        .run_mut(&run.id)
        .unwrap()
        .invocations
        .last_mut()
        .unwrap()
        .prepared["content"] = json!("not the committed content");
    let (request, _) = DefaultContextBuilder
        .build(&invalid, invalid.run(&run.id).unwrap(), tools())
        .await
        .unwrap();
    assert!(restored_pages(&request).is_empty());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn read_recovery_supports_edit_and_verified_completion() {
    if let Err(error) = runtime::Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    struct Recovering {
        requests: std::sync::Mutex<Vec<ModelRequest>>,
    }
    #[async_trait::async_trait]
    impl ModelProvider for Recovering {
        fn identity(&self) -> String {
            "read-loop-recovery".into()
        }
        fn capabilities(&self) -> ModelCapabilities {
            ScriptedProvider::new(vec![]).capabilities()
        }
        async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
            let result = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Tool)
                .and_then(|m| serde_json::from_str::<Value>(&m.content).ok());
            let turn = if result
                .as_ref()
                .is_some_and(|r| r["data"]["verification"]["passed"] == true)
            {
                ScriptedTurn::text("Implemented and verified")
            } else if request
                .messages
                .iter()
                .any(|m| m.content.contains("\"automatic_recovery\":true"))
            {
                assert!(
                    request
                        .messages
                        .iter()
                        .any(|m| m.content.contains("\"automatic_recovery\":true"))
                );
                assert!(
                    request
                        .messages
                        .iter()
                        .any(|m| m.role == Role::Tool && m.content.contains("one"))
                );
                edit("one", "two")
            } else if result
                .as_ref()
                .is_some_and(|r| r["data"]["sha256"] == hash("two"))
            {
                call(
                    "process.run",
                    json!({
                        "argv":["/bin/sh","-c","test \"$(cat game.txt)\" = two"],
                        "verification":{"criteria":["Final fixture content is two"],"paths":["game.txt"]}
                    }),
                )
            } else {
                read("game.txt")
            };
            self.requests.lock().unwrap().push(request);
            Ok(Box::pin(futures_util::stream::iter(
                turn.deltas.into_iter().map(Ok),
            )))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "one").unwrap();
    let provider = Arc::new(Recovering {
        requests: Default::default(),
    });
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let names = ["files.read", "files.edit", "process.run"];
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "coding-basic".into(),
                prompt: "Implement and verify".into(),
                tools: names.map(str::to_owned).to_vec(),
                skills: vec![],
            },
            dir.path(),
            HostPolicy {
                workspace_writable: true,
                tool_permissions: names.map(|n| (n.into(), ToolPermission::Allow)).into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Change one to two and verify it",
            "start",
            Limits {
                max_repeated_calls: 1,
                max_steps: 10,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "two"
    );
    let saved = session.snapshot().await.unwrap();
    assert!(
        saved
            .run(&run.id)
            .unwrap()
            .invocations
            .iter()
            .all(|i| i.result.as_ref().unwrap().status == Outcome::Succeeded)
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 5);
    assert!(requests[2].tools.iter().any(|t| t.name == "files.read"));
    assert!(
        requests[3]
            .tools
            .iter()
            .any(|t| t.name == "process.run" && t.input_schema["x-werk-work-cycle"] == "check")
    );
    assert!(requests[4].tools.iter().any(|t| t.name == "files.read"));
    for request in requests.iter() {
        validate_messages(&request.messages).unwrap();
    }
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
#[tokio::test]
async fn read_recovery_keeps_reads_when_no_edit_or_check_is_requestable() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "one").unwrap();
    let (engine, session, _) =
        fixture(vec![read("game.txt"), read("game.txt")], dir.path(), false).await;
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
    let mut saved = session.snapshot().await.unwrap();
    saved.policy.hard_denied_tools.extend(
        [
            "files.edit",
            "files.write",
            "files.insert",
            "process.run",
            "browser.check",
        ]
        .map(str::to_owned),
    );
    let (request, _) = DefaultContextBuilder
        .build(&saved, saved.run(&run.id).unwrap(), engine.tools())
        .await
        .unwrap();
    assert!(request.tools.iter().any(|t| t.name == "files.read"));
    engine.close().await.unwrap();
}

#[tokio::test]
async fn bounded_read_recovery_can_verify_without_forcing_an_edit() {
    if let Err(error) = runtime::Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "one").unwrap();
    // The model can switch to verification before exhausting recovery.
    let mut turns: Vec<_> = (0..5).map(|_| read("game.txt")).collect();
    turns.extend([
        call(
            "process.run",
            json!({
                "argv":["/bin/sh","-c","test \"$(cat game.txt)\" = one"],
                "verification":{"criteria":["Existing content is one"],"paths":["game.txt"]}
            }),
        ),
        ScriptedTurn::text("Verified existing content"),
    ]);
    let (engine, session, provider) = fixture(turns, dir.path(), false).await;
    let run = session
        .submit(
            "Verify the existing file",
            "start",
            Limits::continuous(),
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "one"
    );
    let requests = provider.requests();
    assert!(requests[5].tools.iter().any(|t| t.name == "files.read"));
    assert!(requests[6].tools.iter().any(|t| t.name == "files.read"));
    assert!(
        !requests[6]
            .messages
            .iter()
            .any(|m| m.content.contains("\"automatic_recovery\":true"))
    );
    engine.close().await.unwrap();
}

struct OfflineReadReplay;
#[async_trait::async_trait]
impl ModelProvider for OfflineReadReplay {
    fn identity(&self) -> String {
        "offline-read-recovery-fixture".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn estimate_input_tokens(&self, request: &ModelRequest) -> Result<usize> {
        // Same offline approximation as the context-planning replay. This
        // validates recovery context, not a live tokenizer or image budget.
        let mut text = request.clone();
        let images: usize = text
            .messages
            .iter_mut()
            .map(|m| {
                let count = m.images.len();
                m.images.clear();
                count
            })
            .sum();
        Ok(serde_json::to_vec(&text)?.len() / 4 + 256 + images * 443)
    }
    async fn stream(&self, _: ModelRequest) -> Result<ModelStream> {
        panic!("offline replay never generates")
    }
}

#[tokio::test]
#[ignore = "explicit read-only replay of a saved read loop; no model or workspace execution"]
async fn saved_read_loop_recovers_an_actionable_request() {
    let path = std::env::var("WW_REPLAY_DATABASE").expect("WW_REPLAY_DATABASE");
    let id = std::env::var("WW_REPLAY_SESSION").expect("WW_REPLAY_SESSION");
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let raw: String = db
        .query_row("SELECT data FROM sessions WHERE id = ?1", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    let saved: SessionData = serde_json::from_str(&raw).unwrap();
    let canonical = serde_json::to_value(&saved).unwrap();
    let run = saved.runs.last().unwrap();
    assert_eq!(run.error.as_ref().unwrap().code, "repetition_limit");
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
        .build()
        .await
        .unwrap();
    let tools = engine
        .tools()
        .into_iter()
        .filter(|t| saved.profile.tools.contains(&t.name) && saved.policy.can_request(t))
        .collect();
    let (request, snapshot) = DefaultContextBuilder
        .build_with_provider(&saved, run, tools, &OfflineReadReplay)
        .await
        .unwrap();
    assert!(
        request
            .tools
            .iter()
            .all(|t| matches!(t.name.as_str(), "process.run" | "interaction.ask"))
    );
    let check = request
        .tools
        .iter()
        .find(|t| t.name == "process.run")
        .unwrap();
    assert!(
        check.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "verification")
    );
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.content.contains("\"automatic_recovery\":true"))
    );
    let receipt = run
        .invocations
        .iter()
        .rev()
        .find(|i| i.call.name == "files.read")
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    let hash = receipt.data["sha256"].as_str().unwrap();
    let content = receipt.data["content"].as_str().unwrap();
    let prefix: String = content.chars().take(100).collect();
    let escaped = serde_json::to_string(&prefix).unwrap();
    assert!(request.messages.iter().any(|m| m.content.contains(hash) && m.content.contains(&escaped[1..escaped.len()-1])));
    assert_eq!(serde_json::to_value(&saved).unwrap(), canonical);
    validate_messages(&request.messages).unwrap();
    let pages = restored_pages(&request);
    for needle in ["const SHAPES =", "const state =", "function spawnDust("] {
        assert!(
            pages.iter().any(|p| p["path"] == "game.js"
                && p["content"].as_str().is_some_and(|s| s.contains(needle))),
            "missing working code: {needle}"
        );
    }
    assert!(pages.iter().any(|p| {
        p["path"] == "index.html"
            && p["content"]
                .as_str()
                .is_some_and(|s| s.contains("src=\"game.js\""))
    }));
    assert!(
        snapshot.estimated_tokens + request.max_output_tokens + 512 <= run.limits.context_tokens
    );
    eprintln!(
        "saved read loop: {} working pages; {} input tokens (offline estimate); behavioral check requested; history unchanged",
        pages.len(),
        snapshot.estimated_tokens
    );
    engine.close().await.unwrap();
}

async fn fixture(
    turns: Vec<ScriptedTurn>,
    dir: &std::path::Path,
    denied: bool,
) -> (WorkerEngine, SessionHandle, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider::new(turns));
    // Opt-in scripted replacements use the actual obligation ID advertised in
    // model context, just as a model correcting a broken check would do.
    struct ReplacementChecks(Arc<ScriptedProvider>);
    #[async_trait::async_trait]
    impl ModelProvider for ReplacementChecks {
        fn identity(&self) -> String {
            self.0.identity()
        }
        fn capabilities(&self) -> ModelCapabilities {
            self.0.capabilities()
        }
        fn concurrency(&self) -> InferenceConcurrency {
            self.0.concurrency()
        }
        async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
            use futures_util::StreamExt;
            let source = request
                .messages
                .iter()
                .filter_map(|message| serde_json::from_str::<Value>(&message.content).ok())
                .find(|value| value["event"] == "coding_task_state")
                .map(|value| {
                    value["data"]["work_cycle"]["unresolved_check"]["source_invocation"].clone()
                })
                .unwrap_or(Value::Null);
            let stream = self.0.stream(request).await?;
            Ok(Box::pin(stream.map(move |delta| {
                delta.map(|mut delta| {
                    if let ModelDelta::Tool { arguments, .. } = &mut delta {
                        let mut args: Value = serde_json::from_str(arguments).unwrap();
                        if args["verification"]["replaces"] == "$unresolved-check" {
                            assert!(
                                source.is_string(),
                                "Replacement requires a retained failed check"
                            );
                            args["verification"]["replaces"] = source.clone();
                            *arguments = args.to_string();
                        }
                    }
                    delta
                })
            })))
        }
    }
    let engine = WorkerEngine::builder(Arc::new(ReplacementChecks(provider.clone())))
        .build()
        .await
        .unwrap();
    let names = [
        "files.read",
        "files.list",
        "files.search",
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

fn failing_check(n: usize) -> ScriptedTurn {
    call(
        "process.run",
        json!({
            "argv":["/bin/sh","-c",format!("printf 'TypeError: g.board is undefined\\n' >&2; exit 1 # attempt {n}")],
            "verification":{"criteria":[format!("Check the state mismatch, attempt {n}")],"paths":["game.txt"]}
        }),
    )
}

#[tokio::test]
async fn repeated_failure_survives_searches_new_reads_noop_edits_compaction_and_resume() {
    if let Err(error) = runtime::Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let content = "one\ntwo\nthree\nfour\nfive\nsix\n";
    std::fs::write(dir.path().join("game.txt"), content).unwrap();
    let mut turns = Vec::new();
    for n in 1..=3 {
        turns.extend([
            call(
                "files.search",
                json!({"path":"game.txt","query":format!("missing-symbol-{n}")}),
            ),
            call(
                "files.read",
                json!({"path":"game.txt","start_line":n,"end_line":n}),
            ),
            failing_check(n),
        ]);
    }
    let steps = turns.len() as u32;
    let (engine, session, _) = fixture(turns, dir.path(), false).await;
    let run = session
        .submit(
            "Fix the mismatch",
            "start",
            Limits {
                max_steps: steps,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
    let mut saved = session.snapshot().await.unwrap();
    let indices = saved
        .run(&run.id)
        .unwrap()
        .invocations
        .iter()
        .flat_map(|i| [i.message_index, i.message_index + 1])
        .collect();
    saved.compactions.push(CompactionRecord {
        id: id("compact"),
        run_id: run.id.clone(),
        attempt_id: id("attempt"),
        message_indices: indices,
        source_hash: "fixture".into(),
        summary: "The agent keeps diagnosing a state mismatch.".into(),
    });
    let persisted = saved.run_mut(&run.id).unwrap();
    persisted.status = RunStatus::Interrupted;
    persisted.error = None;
    persisted.limits = Limits::continuous();
    let store = Arc::new(werk_worker::sessions::InMemoryStore::default());
    use werk_worker::sessions::SessionStore;
    store.save(&saved, None).await.unwrap();
    engine.close().await.unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        call(
            "files.edit",
            json!({"path":"game.txt","expected_hash":hash(content),"old":"one","new":"one"}),
        ),
        failing_check(4),
        call(
            "files.search",
            json!({"path":"game.txt","query":"another diagnosis"}),
        ),
        call(
            "files.read",
            json!({"path":"game.txt","start_line":4,"end_line":4}),
        ),
        failing_check(5),
        call(
            "files.write",
            json!({"path":"must-not-run","content":"bad"}),
        ),
    ]));
    let fresh = WorkerEngine::builder(provider.clone())
        .store(store)
        .build()
        .await
        .unwrap();
    let resumed = fresh.resume_run(&saved.id, &run.id).await.unwrap();
    let result = resumed.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    let error = result.error.unwrap();
    assert_eq!(error.code, "check_no_progress");
    assert!(error.message.contains("5 times"));
    assert!(error.message.contains("g.board is undefined"));
    assert_eq!(provider.requests().len(), 5);
    let warning = provider.requests()[0]
        .messages
        .iter()
        .find_map(|m| {
            serde_json::from_str::<Value>(&m.content)
                .ok()
                .filter(|v| v["event"] == "check_progress")
        })
        .unwrap();
    assert_eq!(warning["data"]["same_failure_occurrences"], 3);
    assert_eq!(warning["data"]["remaining_attempts"], 2);
    assert!(!dir.path().join("must-not-run").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        content
    );
    fresh.close().await.unwrap();
}

#[tokio::test]
async fn actual_file_change_rearms_failure_recovery() {
    if let Err(error) = runtime::Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "one").unwrap();
    let (engine,session,provider)=fixture(vec![
        failing_check(1),failing_check(2),edit("one","two"),failing_check(3),failing_check(4),
        call("process.run",json!({"argv":["/bin/sh","-c","test \"$(cat game.txt)\" = two"],"verification":{"kind":"behavior","replaces":"$unresolved-check","criteria":["Corrected state"],"paths":["game.txt"]}})),
        ScriptedTurn::text("Verified the correction"),
    ],dir.path(),false).await;
    let run = session
        .submit(
            "Fix the state",
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
    assert!(
        !provider.requests()[3]
            .messages
            .iter()
            .any(|m| m.content.contains("\"event\":\"check_progress\""))
    );
    assert!(
        !provider
            .requests()
            .last()
            .unwrap()
            .messages
            .iter()
            .any(|m| m.content.contains("\"event\":\"check_progress\""))
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn new_diagnostic_results_restore_reads_without_requiring_file_changes() {
    if let Err(error) = runtime::Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "one").unwrap();
    let mut turns = vec![read("game.txt"), read("game.txt")];
    for n in 1..=3 {
        turns.extend([
            call("process.run",json!({
                "argv":["/bin/sh","-c",format!("printf 'Different diagnostic {n}\\n' >&2; exit 1")],
                "verification":{"criteria":["Inspect the failure"],"paths":["game.txt"]}
            })),
            read("game.txt"),read("game.txt"),
        ]);
    }
    turns.extend([
        // The public schema permits a legacy check without kind. Its explicit
        // replacement must be honored once that verification succeeds.
        call("process.run",json!({"argv":["/bin/sh","-c","test \"$(cat game.txt)\" = one"],"verification":{"replaces":"$unresolved-check","criteria":["Existing state is valid"],"paths":["game.txt"]}})),
        ScriptedTurn::text("Verified existing state"),
    ]);
    let (engine, session, _) = fixture(turns, dir.path(), false).await;
    let run = session
        .submit(
            "Investigate the state",
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
    engine.close().await.unwrap();
}

#[tokio::test]
async fn continuous_read_loop_stops_and_preserves_the_last_receipt() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("game.txt"), "one").unwrap();
    let (engine, session, provider) = fixture(
        (0..12).map(|_| read("game.txt")).collect(),
        dir.path(),
        false,
    )
    .await;
    let run = session
        .submit("Fix the state", "start", Limits::continuous(), None)
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.error.unwrap().code, "read_no_progress");
    assert_eq!(provider.requests().len(), 6);
    let saved = session.snapshot().await.unwrap();
    let invocations = &saved.run(&run.id).unwrap().invocations;
    assert_eq!(invocations.len(), 6);
    assert_eq!(
        invocations[5].result.as_ref().unwrap().data["content"],
        "one"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "one"
    );
    engine.close().await.unwrap();
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
        edit("one","two"),
        // Review the first recovery increment; its concrete failure permits repair.
        call("process.run",json!({"argv":["/bin/sh","-c","test \"$(cat game.txt)\" = three"],"verification":{"criteria":["Final fixture content is three"],"paths":["game.txt"]}})),
        read("game.txt"), read("game.txt"), edit("two","three"),
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
    assert!(invocations[5].result.as_ref().unwrap().data["verification"]["passed"] == false);
    assert!(
        provider.requests()[6]
            .messages
            .last()
            .unwrap()
            .content
            .contains("Runtime work-cycle recovery"),
        "a failed recovery review must keep goal-focused repair active"
    );
    assert!(invocations[9].result.as_ref().unwrap().data["verification"]["passed"] == true);
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
        let steps = turns.len() as u32;
        let (engine, session, provider) = fixture(turns, dir.path(), denied).await;
        let run = session
            .submit(
                "Inspect and implement",
                "start",
                Limits {
                    max_repeated_calls: 1,
                    max_steps: steps,
                    ..Limits::default()
                },
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(
            result.error.unwrap().code,
            if denied {
                "repetition_limit"
            } else {
                "read_no_progress"
            }
        );
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
            assert!(
                provider
                    .requests()
                    .last()
                    .unwrap()
                    .tools
                    .iter()
                    .any(|t| t.name == "files.read")
            );
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

#[tokio::test]
#[ignore = "read-only replay of a pre-implementation read loop; set WW_REPLAY_SNAPSHOT"]
async fn saved_initial_read_loop_keeps_code_when_switching_to_html() {
    let raw =
        std::fs::read(std::env::var("WW_REPLAY_SNAPSHOT").expect("WW_REPLAY_SNAPSHOT")).unwrap();
    let saved: SessionData = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        saved.runs.last().unwrap().error.as_ref().unwrap().code,
        "read_no_progress"
    );
    let canonical = serde_json::to_value(&saved).unwrap();
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
        .build()
        .await
        .unwrap();
    // Both the first recovery decision (after inspecting HTML again) and the
    // persisted stopped run must retain the JS interfaces without any edit.
    for count in [13, 15] {
        let mut prefix = saved.clone();
        let run = prefix.runs.last_mut().unwrap();
        run.invocations.truncate(count);
        assert!(run.invocations.iter().all(|i| !matches!(
            i.call.name.as_str(),
            "files.write" | "files.edit" | "files.insert"
        )));
        let end = run.invocations.last().unwrap().message_index + 2;
        prefix.messages.truncate(end);
        run.contexts.retain(|c| {
            c.source
                .as_ref()
                .is_some_and(|source| source.message_count <= end)
        });
        let tools = engine
            .tools()
            .into_iter()
            .filter(|t| prefix.profile.tools.contains(&t.name) && prefix.policy.can_request(t))
            .collect();
        let (request, snapshot) = DefaultContextBuilder
            .build_with_provider(
                &prefix,
                prefix.runs.last().unwrap(),
                tools,
                &OfflineReadReplay,
            )
            .await
            .unwrap();
        assert!(
            request
                .messages
                .last()
                .unwrap()
                .content
                .contains("before the first implementation change")
        );
        assert!(request.tools.iter().any(|t| t.name == "files.edit"));
        assert!(request.tools.iter().any(|t| t.name == "files.read"));
        let mut pages = restored_pages(&request);
        // Recent receipts need no restored duplicate: both forms are usable
        // file evidence at this decision boundary.
        pages.extend(
            request
                .messages
                .iter()
                .filter(|m| m.role == Role::Tool)
                .filter_map(|m| serde_json::from_str::<Value>(&m.content).ok())
                .map(|v| v["data"].clone()),
        );
        for needle in ["const SHAPES =", "const state =", "function spawnDust("] {
            assert!(
                pages.iter().any(|p| p["path"] == "game.js"
                    && p["content"].as_str().is_some_and(|s| s.contains(needle))),
                "missing {needle} after {count} calls"
            );
        }
        assert!(pages.iter().any(|p| {
            p["path"] == "index.html"
                && p["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("src=\"game.js\""))
        }));
        assert!(
            request
                .messages
                .iter()
                .filter(|m| !m.tool_calls.is_empty())
                .count()
                <= 2
        );
        assert!(
            snapshot.estimated_tokens + request.max_output_tokens + 512
                <= prefix.runs.last().unwrap().limits.context_tokens
        );
        validate_messages(&request.messages).unwrap();
        eprintln!(
            "initial-read replay after {count} calls: {} working pages, {} input tokens (offline estimate), original tools retained",
            pages.len(),
            snapshot.estimated_tokens
        );
    }
    assert_eq!(serde_json::to_value(&saved).unwrap(), canonical);
    engine.close().await.unwrap();
}

#[tokio::test]
async fn complete_prepared_file_survives_before_its_first_read() {
    let dir = tempfile::tempdir().unwrap();
    let content = format!(
        "{}\nconst sharedInterface = 'middle-contract';\n{}\n",
        "a".repeat(7000),
        "z".repeat(3000)
    );
    let first = format!("{}NEXT_ANCHOR", "a".repeat(3000));
    let second = format!("{}NEXT_ANCHOR", "a".repeat(6000));
    let tail = format!(
        "{}\nconst sharedInterface = 'middle-contract';\n{}\n",
        "a".repeat(1000),
        "z".repeat(3000)
    );
    let (engine, session, _) = fixture(vec![
        call("files.write", json!({"path":"game.js","content":first})),
        call("files.edit", json!({"path":"game.js","expected_hash":hash(&first),"old":"NEXT_ANCHOR","new":format!("{}NEXT_ANCHOR", "a".repeat(3000))})),
        call("files.edit", json!({"path":"game.js","expected_hash":hash(&second),"old":"NEXT_ANCHOR","new":tail})),
    ], dir.path(), false).await;
    let run = session
        .submit(
            "Build the game",
            "start",
            Limits {
                context_tokens: 65536,
                max_steps: 3,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
    let mut saved = session.snapshot().await.unwrap();
    assert_eq!(
        saved.run(&run.id).unwrap().invocations[0]
            .result
            .as_ref()
            .unwrap()
            .status,
        Outcome::Succeeded,
        "{:?}",
        saved.run(&run.id).unwrap().invocations[0].result
    );

    saved.messages.retain(|m| m.role == Role::User);
    saved.run_mut(&run.id).unwrap().contexts.clear();
    let (request, _) = DefaultContextBuilder
        .build(&saved, saved.run(&run.id).unwrap(), engine.tools())
        .await
        .unwrap();
    let mut pages = restored_pages(&request);
    pages.sort_by_key(|p| p["start_byte"].as_u64().unwrap());
    assert!(pages.iter().all(|p| p["sha256"] == hash(&content)));
    let actual: String = pages
        .iter()
        .map(|p| p["content"].as_str().unwrap())
        .collect();
    assert_eq!(
        actual, content,
        "A fully known 10 KiB file must not lose its middle or require rereading"
    );
    assert_eq!(saved.run(&run.id).unwrap().invocations.len(), 3);
    validate_messages(&request.messages).unwrap();
    engine.close().await.unwrap();
}
