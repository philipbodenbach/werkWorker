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
async fn browser_frames_compact_without_losing_original_user_image_or_checkpoint() {
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
            .any(|m| m.content.contains("browser_checkpoint") && m.content.contains("obs23"))
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
