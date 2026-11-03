//! Planning cost regressions: counters run locally, with no model or credentials.
use async_trait::async_trait;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use werk_worker::{
    context::{ContextBuilder, DefaultContextBuilder},
    policy::HostPolicy,
    profiles::AgentProfile,
    providers::*,
    *,
};

#[derive(Default)]
struct Counter {
    calls: AtomicUsize,
}
#[async_trait]
impl ModelProvider for Counter {
    fn identity(&self) -> String {
        "offline-planning-fixture".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn count_input_tokens(&self, request: &ModelRequest) -> Result<Option<usize>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        // Deliberately no memoization: count every planning round, including duplicates.
        let mut text = request.clone();
        let images: usize = text
            .messages
            .iter_mut()
            .map(|m| {
                let n = m.images.len();
                m.images.clear();
                n
            })
            .sum();
        Ok(Some(
            serde_json::to_vec(&text)?.len() / 4 + 256 + images * 443,
        ))
    }
    async fn stream(&self, _: ModelRequest) -> Result<ModelStream> {
        panic!("offline counter never generates")
    }
}

async fn history(groups: usize) -> SessionData {
    let dir = tempfile::tempdir().unwrap();
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
        "seed",
    )])))
    .build()
    .await
    .unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "fixture".into(),
                prompt: "Retain constraints".into(),
                tools: vec![],
                skills: vec![],
            },
            dir.path(),
            HostPolicy::default(),
            serde_json::Value::Null,
        )
        .await
        .unwrap();
    session
        .submit("Keep all user constraints", "seed", Limits::default(), None)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    let mut state = session.snapshot().await.unwrap();
    engine.close().await.unwrap();
    state.messages.truncate(1);
    state.runs[0].contexts.clear();
    state.runs[0].limits.context_tokens = 16384;
    state.runs[0].limits.reserve_output_tokens = 4096;
    for n in 0..groups {
        let call = ToolCall {
            id: format!("read-{n}"),
            name: "files.read".into(),
            arguments: serde_json::json!({"path":format!("file-{n}.js")}),
        };
        let mut assistant = Message::text(Role::Assistant, format!("Read file {n}"));
        assistant.tool_calls.push(call.clone());
        state.messages.push(assistant);
        let mut result = Message::text(
            Role::Tool,
            format!("file-{n}: {}", "observed code\n".repeat(700)),
        );
        result.tool_call_id = Some(call.id);
        state.messages.push(result);
    }
    state
}

#[tokio::test]
async fn coding_plan_updates_keep_authorized_browser_tools_available() {
    use serde_json::json;
    use werk_worker::policy::ToolPermission;
    use werk_worker::tools::ToolRegistry;
    let mut state = history(0).await;
    state.profile.name = "coding-basic".into();
    state.policy.workspace_writable = true;
    state.policy.browser = Some(Default::default());
    let registry = ToolRegistry::builtins().unwrap();
    state.profile.tools = ["files.write", "task.plan", "interaction.ask"]
        .into_iter()
        .chain(werk_worker::browser_policy::TOOLS.iter().copied())
        .map(str::to_owned)
        .collect();
    for name in &state.profile.tools {
        state
            .policy
            .tool_permissions
            .insert(name.clone(), ToolPermission::Allow);
    }
    let definitions: Vec<_> = state
        .profile
        .tools
        .iter()
        .map(|name| registry.get(name).unwrap().definition())
        .collect();
    for detailed in [true, false] {
        state.runs[0].plan = if detailed {
            json!({"steps":[{"id":"draw","description":"Draw a house","status":"in_progress","acceptance_criteria":["Red roof visible"],"verification_method":"Inspect screenshot"}]})
        } else {
            // Reproduce the saved run: a later plan update omits coding-specific
            // criteria while the browser remains authorized and open.
            json!({"steps":[{"id":"draw","description":"Draw a red roof","status":"in_progress"}]})
        };
        let run = state.runs[0].clone();
        state.metadata["_werkworker_harness"][&run.id]["config"]["mode"] = "adaptive".into();
        for action_allowed in [true, false] {
            state.policy.tool_permissions.insert(
                "browser.action".into(),
                if action_allowed {
                    ToolPermission::Allow
                } else {
                    ToolPermission::Deny
                },
            );
            let offered: Vec<_> = definitions
                .iter()
                .filter(|d| state.policy.can_request(d))
                .cloned()
                .collect();
            let (request, _) = DefaultContextBuilder
                .build_with_provider(&state, &run, offered, &Counter::default())
                .await
                .unwrap();
            for name in werk_worker::browser_policy::TOOLS {
                let actual = request.tools.iter().find(|t| t.name == *name);
                assert_eq!(
                    actual.is_some(),
                    *name != "browser.action" || action_allowed,
                    "{name}, detailed={detailed}"
                );
                if let Some(tool) = actual {
                    assert_eq!(
                        tool.input_schema,
                        registry.get(name).unwrap().definition().input_schema
                    );
                }
            }
            assert_eq!(
                request.tools.iter().any(|t| t.name == "files.write"),
                detailed
            );
            if !detailed {
                let plan = request
                    .tools
                    .iter()
                    .find(|t| t.name == "task.plan")
                    .unwrap();
                assert_eq!(plan.input_schema["x-werk-work-cycle"], "plan");
            }
        }
    }
}

#[tokio::test]
async fn explicit_finite_review_limits_reserve_synthesis_but_continuous_reviews_do_not() {
    use werk_worker::{agents::ParentLink, tools::ToolRegistry};
    let mut state = history(0).await;
    state.parent = Some(ParentLink {
        session_id: "parent".into(),
        run_id: "parent_run".into(),
        root_session: "parent".into(),
        root_run: "parent_run".into(),
        depth: 1,
    });
    state.policy.workspace_writable = false;
    state.profile.tools = vec!["files.read".into()];
    state.runs[0].limits.max_model_requests = 6;
    state.runs[0].metrics.model_requests = 5;
    let definitions = vec![
        ToolRegistry::builtins()
            .unwrap()
            .get("files.read")
            .unwrap()
            .definition(),
    ];
    let (request, _) = DefaultContextBuilder
        .build_with_provider(
            &state,
            &state.runs[0],
            definitions.clone(),
            &Counter::default(),
        )
        .await
        .unwrap();
    assert!(request.tools.is_empty());
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.content.contains("delegated_report_due"))
    );
    state.runs[0].limits.continuous = true;
    state.runs[0].metrics.model_requests = 50;
    let (request, _) = DefaultContextBuilder
        .build_with_provider(&state, &state.runs[0], definitions, &Counter::default())
        .await
        .unwrap();
    assert!(!request.tools.is_empty());
    assert!(
        !request
            .messages
            .iter()
            .any(|m| m.content.contains("delegated_report_due"))
    );
}

#[tokio::test]
async fn long_history_planning_has_bounded_counts_and_reuses_selection_after_resume() {
    let mut state = history(120).await;
    let original = state.messages.clone();
    let counter = Counter::default();
    let (request, snapshot) = DefaultContextBuilder
        .build_with_provider(&state, &state.runs[0], vec![], &counter)
        .await
        .unwrap();
    let cold = counter.calls.swap(0, Ordering::Relaxed);
    eprintln!(
        "cold counts={cold}, omitted={}, retained={}",
        snapshot.omitted_messages.len(),
        request.messages.len()
    );
    assert!(cold <= 12, "cold planning made {cold} counts");
    assert!(snapshot.omitted_messages.len() > 200);
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.content.contains("file-119:"))
    );
    assert!(snapshot.estimated_tokens + request.max_output_tokens + 512 <= 16384);
    state.runs[0].contexts.push(snapshot);
    state
        .messages
        .push(Message::text(Role::User, "New steering: one HTML file"));
    // Round-trip exactly as a durable session resume would, without persisting user data.
    let state: SessionData = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    let (request, _) = DefaultContextBuilder
        .build_with_provider(&state, &state.runs[0], vec![], &counter)
        .await
        .unwrap();
    let warm = counter.calls.load(Ordering::Relaxed);
    eprintln!("resumed counts={warm}");
    assert_eq!(
        warm, 1,
        "unchanged working context should need one admission count"
    );
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.content == "New steering: one HTML file")
    );
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.content == "Keep all user constraints")
    );
    assert_eq!(
        serde_json::to_value(&state.messages[..original.len()]).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    context::validate_messages(&request.messages).unwrap();
}

#[tokio::test]
#[ignore = "explicit read-only replay of a local saved session; no live server"]
async fn saved_session_planning_replay() {
    let data: String = if let Ok(path) = std::env::var("WW_REPLAY_SNAPSHOT") {
        std::fs::read_to_string(path).unwrap()
    } else {
        let path = std::env::var("WW_REPLAY_DATABASE").expect("WW_REPLAY_DATABASE");
        let id = std::env::var("WW_REPLAY_SESSION").expect("WW_REPLAY_SESSION");
        let connection =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        connection
            .query_row("SELECT data FROM sessions WHERE id = ?1", [&id], |row| {
                row.get(0)
            })
            .unwrap()
    };
    let mut session: SessionData = serde_json::from_str(&data).unwrap();
    let index = session.runs.len() - 1;
    // Freeze effective limits as in the reported run. Fixture counts are estimates,
    // not llama.cpp token counts; only planning work is compared here.
    let mut run = session.runs[index].clone();
    run.contexts.clear();
    let counter = Counter::default();
    let (_, snapshot) = DefaultContextBuilder
        .build_with_provider(&session, &run, vec![], &counter)
        .await
        .unwrap();
    let cold = counter.calls.swap(0, Ordering::Relaxed);
    run.contexts.push(snapshot.clone());
    session.runs[index] = run.clone();
    let (_, next) = DefaultContextBuilder
        .build_with_provider(&session, &run, vec![], &counter)
        .await
        .unwrap();
    eprintln!(
        "offline replay: messages={}, cold_counts={cold}, warm_counts={}, omitted={}, input_estimate={}",
        session.messages.len(),
        counter.calls.load(Ordering::Relaxed),
        snapshot.omitted_messages.len(),
        next.estimated_tokens
    );
}

#[tokio::test]
async fn changed_prefix_invalidates_selection_and_lower_limits_are_recounted() {
    let mut state = history(120).await;
    let counter = Counter::default();
    let (_, snapshot) = DefaultContextBuilder
        .build_with_provider(&state, &state.runs[0], vec![], &counter)
        .await
        .unwrap();
    state.runs[0].contexts.push(snapshot);
    state.runs[0].limits.context_tokens = 12000;
    let (request, snapshot) = DefaultContextBuilder
        .build_with_provider(&state, &state.runs[0], vec![], &counter)
        .await
        .unwrap();
    assert!(snapshot.planning.as_ref().unwrap().reused_selection);
    assert!(snapshot.estimated_tokens + request.max_output_tokens + 512 <= 12000);
    state.messages[0]
        .content
        .push_str("; changed canonical constraint");
    let (_, snapshot) = DefaultContextBuilder
        .build_with_provider(&state, &state.runs[0], vec![], &counter)
        .await
        .unwrap();
    assert!(!snapshot.planning.as_ref().unwrap().reused_selection);
    // Old stores/custom builders without a checkpoint are still readable.
    let mut legacy = serde_json::to_value(snapshot).unwrap();
    legacy.as_object_mut().unwrap().remove("source");
    legacy.as_object_mut().unwrap().remove("planning");
    let legacy: ContextSnapshot = serde_json::from_value(legacy).unwrap();
    assert!(legacy.source.is_none());
}

struct NoProgress(AtomicUsize);
#[async_trait]
impl ModelProvider for NoProgress {
    fn identity(&self) -> String {
        "noncompactable-template-fixture".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn count_input_tokens(&self, request: &ModelRequest) -> Result<Option<usize>> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(Some(
            if request
                .messages
                .iter()
                .all(|m| matches!(m.role, Role::System | Role::Developer | Role::User))
            {
                4000
            } else {
                20000
            },
        ))
    }
    async fn stream(&self, _: ModelRequest) -> Result<ModelStream> {
        panic!("no generation")
    }
}

#[tokio::test]
async fn ineffective_compaction_is_detected_and_stopped_without_losing_history() {
    let state = history(120).await;
    let original = fingerprint(&state.messages);
    let counter = NoProgress(AtomicUsize::new(0));
    let error = DefaultContextBuilder
        .build_with_provider(&state, &state.runs[0], vec![], &counter)
        .await
        .unwrap_err();
    assert_eq!(error.code, "compaction_no_progress");
    assert!(!error.retryable);
    assert!(counter.0.load(Ordering::Relaxed) <= 18);
    assert_eq!(original, fingerprint(&state.messages));
}

#[tokio::test]
async fn reused_selection_does_not_duplicate_an_existing_model_summary() {
    let mut state = history(2).await;
    state.compactions.push(CompactionRecord {
        id: "summary".into(),
        run_id: state.runs[0].id.clone(),
        attempt_id: "summary-attempt".into(),
        message_indices: vec![1, 2],
        source_hash: "fixture".into(),
        summary: "UNIQUE_MODEL_SUMMARY Preserve earlier decisions.".into(),
    });
    let counter = Counter::default();
    let (_, snapshot) = DefaultContextBuilder
        .build_with_provider(&state, &state.runs[0], vec![], &counter)
        .await
        .unwrap();
    assert!(
        snapshot
            .summary
            .as_ref()
            .unwrap()
            .contains("UNIQUE_MODEL_SUMMARY")
    );
    state.runs[0].contexts.push(snapshot);
    let (request, _) = DefaultContextBuilder
        .build_with_provider(&state, &state.runs[0], vec![], &counter)
        .await
        .unwrap();
    let text = serde_json::to_string(&request.messages).unwrap();
    assert_eq!(text.matches("UNIQUE_MODEL_SUMMARY").count(), 1);
}

#[tokio::test]
async fn browser_skill_is_present_once_before_and_after_compaction() {
    use serde_json::json;
    let skill = skills::Skill::parse(
        "browser-use",
        "fixture",
        "---\nname: browser-use\ndescription: Browser instructions\n---\nUNIQUE_BROWSER_INSTRUCTIONS Use fresh observations and preserve the user's constraints.",
    ).unwrap();
    for mode in ["visible", "compacted", "unloaded", "truncated"] {
        let mut state = history(0).await;
        state.profile.tools = vec!["browser.open".into()];
        state.profile.skills = vec![skill.name.clone()];
        state
            .skill_snapshots
            .insert(skill.name.clone(), skill.clone());
        let append = |state: &mut SessionData, call: ToolCall, result: ToolResult| {
            let index = state.messages.len();
            let mut assistant = Message::text(Role::Assistant, "");
            assistant.tool_calls.push(call.clone());
            state.messages.push(assistant);
            state.messages.push(Message::tool(&call.id, &result));
            state.runs[0].invocations.push(serde_json::from_value(json!({
                "invocation_id":format!("inv{index}"),"step_id":format!("step{index}"),
                "message_index":index,"call":call,"status":"finished","prepared":{},"result":result
            })).unwrap());
        };
        if mode != "unloaded" {
            let mut result = ToolResult::ok(serde_json::to_value(&skill).unwrap());
            if mode == "truncated" {
                result.truncated = true;
                result.data["body"] = json!("Incomplete instructions");
            }
            append(
                &mut state,
                ToolCall {
                    id: "skill".into(),
                    name: "skills.load".into(),
                    arguments: json!({"name":"browser-use"}),
                },
                result,
            );
        }
        append(
            &mut state,
            ToolCall {
                id: "browser".into(),
                name: "browser.open".into(),
                arguments: json!({"url":"https://example.com"}),
            },
            ToolResult::ok(json!({"url":"https://example.com"})),
        );
        if mode == "compacted" {
            state.compactions.push(CompactionRecord {
                id: "compact".into(),
                run_id: state.runs[0].id.clone(),
                attempt_id: "attempt".into(),
                message_indices: vec![1, 2],
                source_hash: "fixture".into(),
                summary: "Skill loaded".into(),
            });
        }
        let original = fingerprint(&state.messages);
        let tools = vec![
            tools::ToolRegistry::builtins()
                .unwrap()
                .get("browser.open")
                .unwrap()
                .definition(),
        ];
        let (request, _) = DefaultContextBuilder
            .build_with_provider(&state, &state.runs[0], tools, &Counter::default())
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_string(&request.messages)
                .unwrap()
                .matches("UNIQUE_BROWSER_INSTRUCTIONS")
                .count(),
            1,
            "{mode}"
        );
        assert_eq!(
            fingerprint(&state.messages),
            original,
            "canonical history changed"
        );
        context::validate_messages(&request.messages).unwrap();
    }
}

#[tokio::test]
async fn browser_frames_compact_without_losing_original_user_image_or_current_receipt() {
    let mut state = history(0).await;
    state.runs[0].limits.context_tokens = 24000;
    state.runs[0].limits.reserve_output_tokens = 1024;
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(16, 16)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    let image = attachments::ImageAttachment::from_bytes(bytes.get_ref()).unwrap();
    state.messages[0].images = vec![image.clone()];
    for n in 0..24 {
        let call = ToolCall {
            id: format!("browser{n}"),
            name: "browser.observe".into(),
            arguments: serde_json::json!({"browser_id":"browser_live"}),
        };
        let mut assistant = Message::text(Role::Assistant, "Observe");
        assistant.tool_calls.push(call.clone());
        let index = state.messages.len();
        state.messages.push(assistant);
        let mut result = ToolResult::ok(
            serde_json::json!({"browser_id":"browser_live","tab_id":1,"observation_id":format!("obs{n}"),"revision":n,"text":"page detail ".repeat(800)}),
        );
        result.images.push(image.clone());
        state.messages.push(Message::tool(&call.id, &result));
        state.runs[0].invocations.push(serde_json::from_value(serde_json::json!({"invocation_id":format!("inv{n}"),"step_id":format!("step{n}"),"message_index":index,"call":call,"status":"finished","prepared":{},"result":result})).unwrap());
    }
    let (request, snapshot) = DefaultContextBuilder
        .build_with_provider(&state, &state.runs[0], vec![], &Counter::default())
        .await
        .unwrap();
    assert!(!snapshot.omitted_messages.is_empty());
    assert_eq!(
        request
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .unwrap()
            .images,
        vec![image]
    );
    assert!(
        request
            .messages
            .iter()
            .map(|m| m.images.len())
            .sum::<usize>()
            <= 3
    );
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("browser23") && m.content.contains("obs23"))
    );
    assert!(
        !request
            .messages
            .iter()
            .any(|m| m.content.contains("browser_checkpoint")),
        "Do not duplicate the protected full observation in an assistant envelope"
    );
    assert!(
        state.messages.iter().map(|m| m.images.len()).sum::<usize>() > 20,
        "canonical receipts retain event evidence"
    );
    context::validate_messages(&request.messages).unwrap();
    let counted = Counter::default()
        .count_input_tokens(&request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.estimated_tokens, counted);
}

#[tokio::test]
async fn browser_observation_survives_validation_errors_and_saved_compaction() {
    use serde_json::json;
    let mut state = history(12).await;
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(16, 16)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    let image = attachments::ImageAttachment::from_bytes(bytes.get_ref()).unwrap();
    let observation_index = state.messages.len();
    for (id, name, arguments, mut result) in [
        (
            "frame",
            "browser.action",
            json!({"browser_id":"live","action":"click","target":{"label":"missing"}}),
            {
                // Failed actions can still return the freshest valid observation.
                let mut result = ToolResult::error(Outcome::Failed, "browser_action_failed");
                result.data = json!({"browser_id":"live","observation_id":"live:2","revision":2,
                "error":"No matching label","controls":[{"ref":"e2","name":"Fill","bounds":{"x":10,"y":20}}]});
                result.images.push(image.clone());
                result
            },
        ),
        (
            "invalid",
            "browser.action",
            json!({"browser_id":"live","action":"click","x":40,"y":50}),
            ToolResult::error(Outcome::ValidationError, "invalid_browser_action"),
        ),
    ] {
        let call = ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        };
        let index = state.messages.len();
        let mut assistant = Message::text(Role::Assistant, "");
        assistant.tool_calls.push(call.clone());
        state.messages.push(assistant);
        result.data["message"] = json!("Inspect the current frame and correct the arguments");
        state.messages.push(Message::tool(id, &result));
        state.runs[0].invocations.push(serde_json::from_value(json!({"invocation_id":id,"step_id":id,"message_index":index,"call":call,"status":"finished","prepared":{},"result":result})).unwrap());
    }
    let original = fingerprint(&state.messages);
    for pass in 0..2 {
        let (request, mut snapshot) = DefaultContextBuilder
            .build_with_provider(&state, &state.runs[0], vec![], &Counter::default())
            .await
            .unwrap();
        assert!(!snapshot.omitted_messages.is_empty());
        let frame = request
            .messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("frame"))
            .expect("fresh observation must survive a subsequent validation error");
        assert_eq!(frame.images, vec![image.clone()]);
        assert!(frame.content.contains("e2"));
        assert!(!snapshot.omitted_messages.contains(&observation_index));
        assert!(
            request
                .messages
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some("invalid"))
        );
        context::validate_messages(&request.messages).unwrap();
        assert_eq!(fingerprint(&state.messages), original);
        if pass == 0 {
            // A persisted selection from an older CLI already lost this frame.
            snapshot
                .omitted_messages
                .extend([observation_index, observation_index + 1]);
            state.runs[0].contexts.push(snapshot);
        }
    }
}

#[tokio::test]
async fn browser_checkpoint_recovers_receipts_without_a_canonical_message_group() {
    use serde_json::json;
    let mut state = history(0).await;
    state.runs[0].invocations.push(serde_json::from_value(json!({
        "invocation_id":"saved", "step_id":"saved", "message_index":1,
        "call":{"id":"saved", "name":"browser.observe", "arguments":{"browser_id":"previous"}},
        "status":"finished", "prepared":{},
        "result":ToolResult::ok(json!({"browser_id":"previous","observation_id":"previous:4","tab_id":1,"title":"Saved observation"}))
    })).unwrap());
    let (request, _) = DefaultContextBuilder
        .build_with_provider(&state, &state.runs[0], vec![], &Counter::default())
        .await
        .unwrap();
    let checkpoint = request
        .messages
        .iter()
        .find(|m| m.content.contains("browser_checkpoint"))
        .expect("durable receipt still supplies a historical checkpoint after transcript loss");
    assert_eq!(checkpoint.role, Role::Assistant);
    assert!(checkpoint.content.contains("previous:4"));
    assert!(checkpoint.content.contains("Historical receipt"));
    context::validate_messages(&request.messages).unwrap();
}
