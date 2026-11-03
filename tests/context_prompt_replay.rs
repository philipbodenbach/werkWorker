//! Opt-in diagnostics on the same saved history; the database stays read-only.
//! Offline preparation tests use no credentials or network. The admission replay
//! explicitly uses the local Werk counter (optional WW_REPLAY_API_KEY), never
//! generation or browser actions.
//! Use WW_REPLAY_SNAPSHOT, or WW_REPLAY_DATABASE plus WW_REPLAY_SESSION.
//! Capture hashes before a change with WW_PROMPT_CAPTURE; compare afterwards
//! with WW_PROMPT_BASELINE on the same frozen input. Timing includes request
//! hashing, uses the active Cargo build profile, and is not an inference or
//! end-to-end latency measurement. No timing threshold is asserted.
use std::{collections::BTreeMap, time::Instant};
use werk_worker::{
    context::{ContextBuilder, DefaultContextBuilder},
    *,
};

/// Uses only the local admission/token-count API, never generation or tools.
#[tokio::test]
#[ignore = "set WW_REPLAY_DATABASE/WW_REPLAY_SESSION/WW_REPLAY_MODEL; requires local Werk token counter"]
async fn saved_browser_admission_replay() {
    use werk_worker::{providers::*, tools::ToolRegistry};
    let connection = rusqlite::Connection::open_with_flags(
        std::env::var("WW_REPLAY_DATABASE").expect("WW_REPLAY_DATABASE"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let text: String = connection
        .query_row(
            "SELECT data FROM sessions WHERE id = ?1",
            [std::env::var("WW_REPLAY_SESSION").expect("WW_REPLAY_SESSION")],
            |row| row.get(0),
        )
        .unwrap();
    let session: SessionData = serde_json::from_str(&text).unwrap();
    let original = fingerprint(&session);
    let run = session.runs.last().unwrap();
    let registry = ToolRegistry::builtins().unwrap();
    let tools = session
        .profile
        .tools
        .iter()
        .map(|name| registry.get(name).unwrap().definition())
        .filter(|def| session.policy.can_request(def))
        .collect();
    let provider = WerkProvider::connect(
        WerkConfig::local(std::env::var("WW_REPLAY_MODEL").expect("WW_REPLAY_MODEL")),
        std::env::var("WW_REPLAY_API_KEY").ok(),
    )
    .await
    .unwrap();
    let (request, snapshot) = DefaultContextBuilder
        .build_with_provider(&session, run, tools, &provider)
        .await
        .unwrap();
    eprintln!(
        "saved browser admission: input={}, output={}, tools={}, messages={}, images={}, counts={}",
        snapshot.estimated_tokens,
        request.max_output_tokens,
        request.tools.len(),
        request.messages.len(),
        request
            .messages
            .iter()
            .map(|m| m.images.len())
            .sum::<usize>(),
        snapshot.planning.as_ref().unwrap().provider_estimates
    );
    assert!(
        snapshot.estimated_tokens + request.max_output_tokens + 512 <= run.limits.context_tokens
    );
    let current_frame = session
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool && !m.images.is_empty())
        .unwrap();
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.tool_call_id == current_frame.tool_call_id
                && m.images == current_frame.images)
    );
    for constraint in session
        .messages
        .iter()
        .filter(|m| matches!(m.role, Role::User | Role::System | Role::Developer))
    {
        assert!(request.messages.contains(constraint));
    }
    context::validate_messages(&request.messages).unwrap();
    assert_eq!(fingerprint(&session), original);
}

#[tokio::test]
#[ignore = "set WW_REPLAY_SNAPSHOT or WW_REPLAY_DATABASE/WW_REPLAY_SESSION; optional WW_PROMPT_BASELINE/WW_PROMPT_CAPTURE"]
async fn saved_prompt_preparation_replay() {
    let data: String = if let Ok(path) = std::env::var("WW_REPLAY_SNAPSHOT") {
        std::fs::read_to_string(path).unwrap()
    } else {
        let connection = rusqlite::Connection::open_with_flags(
            std::env::var("WW_REPLAY_DATABASE").expect("WW_REPLAY_DATABASE"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        connection
            .query_row(
                "SELECT data FROM sessions WHERE id = ?1",
                [std::env::var("WW_REPLAY_SESSION").expect("WW_REPLAY_SESSION")],
                |row| row.get(0),
            )
            .unwrap()
    };
    let saved: SessionData = serde_json::from_str(&data).unwrap();
    let mut hashes = BTreeMap::new();
    for mode in ["classic", "minimal", "adaptive"] {
        let mut session = saved.clone();
        let run = session.runs.last_mut().unwrap();
        run.contexts.clear();
        // Widen the planning budget. Very large snapshots can still compact;
        // no provider is invoked, and the canonical stored limits stay untouched.
        run.limits.context_tokens = 1_000_000;
        let run = run.clone();
        session.metadata["_werkworker_harness"][&run.id]["config"]["mode"] = mode.into();
        let start = Instant::now();
        let mut expected = None;
        for _ in 0..5 {
            let (request, _) = DefaultContextBuilder
                .build(&session, &run, vec![])
                .await
                .unwrap();
            let hash = fingerprint(&request);
            if let Some(expected) = &expected {
                assert_eq!(&hash, expected, "request changed during {mode} replay");
            }
            expected = Some(hash);
        }
        eprintln!(
            "prompt replay mode={mode}, invocations={}, mean_ms={:.3}",
            run.invocations.len(),
            start.elapsed().as_secs_f64() * 200.0
        );
        hashes.insert(mode, expected.unwrap());
    }
    if let Ok(path) = std::env::var("WW_PROMPT_BASELINE") {
        let baseline: BTreeMap<String, String> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(&hashes).unwrap(),
            serde_json::to_value(baseline).unwrap()
        );
    }
    if let Ok(path) = std::env::var("WW_PROMPT_CAPTURE") {
        // Store hashes only: the replay must not duplicate private transcript data.
        std::fs::write(path, serde_json::to_vec_pretty(&hashes).unwrap()).unwrap();
    }
}

/// Rebuild the request immediately before a saved browser question.
/// No inference, credentials or browser actions are involved.
#[tokio::test]
#[ignore = "set WW_REPLAY_DATABASE/WW_REPLAY_SESSION to a saved browser question"]
async fn saved_browser_capability_replay() {
    use werk_worker::tools::ToolRegistry;
    let connection = rusqlite::Connection::open_with_flags(
        std::env::var("WW_REPLAY_DATABASE").expect("WW_REPLAY_DATABASE"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let text: String = connection
        .query_row(
            "SELECT data FROM sessions WHERE id = ?1",
            [std::env::var("WW_REPLAY_SESSION").expect("WW_REPLAY_SESSION")],
            |row| row.get(0),
        )
        .unwrap();
    let mut session: SessionData = serde_json::from_str(&text).unwrap();
    let run = session.runs.last_mut().unwrap();
    let pending = run
        .invocations
        .iter()
        .rposition(|i| i.call.name == "interaction.ask")
        .expect("saved interaction.ask");
    session
        .messages
        .truncate(run.invocations[pending].message_index);
    run.invocations.truncate(pending);
    run.status = RunStatus::Running;
    run.contexts.clear();
    let run = run.clone();
    let registry = ToolRegistry::builtins().unwrap();
    let tools: Vec<_> = session
        .profile
        .tools
        .iter()
        .map(|name| registry.get(name).unwrap().definition())
        .filter(|def| session.policy.can_request(def))
        .collect();
    let (request, _) = DefaultContextBuilder
        .build(&session, &run, tools)
        .await
        .unwrap();
    for name in ["browser.open", "browser.observe", "browser.action"] {
        assert!(
            request.tools.iter().any(|t| t.name == name),
            "{name} missing from offered tools"
        );
    }
    eprintln!(
        "browser replay: {} offered tools, {} image(s), {} messages; source database unchanged",
        request.tools.len(),
        request
            .messages
            .iter()
            .map(|m| m.images.len())
            .sum::<usize>(),
        request.messages.len()
    );
}
