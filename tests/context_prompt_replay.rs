//! Opt-in diagnostics on the same saved history; the database stays read-only.
//! Offline preparation tests use no credentials or network. The admission replay
//! explicitly uses the local Werk counter (optional WW_REPLAY_API_KEY), never
//! generation or browser actions. The separate target-decision probe performs
//! one inference against a captured request and never executes returned tools.
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

#[path = "support/browser_controls.rs"]
mod browser_controls;
use browser_controls::expanded_controls;

struct AdmissionCapture {
    provider: werk_worker::providers::WerkProvider,
    best: std::sync::Mutex<Option<(usize, werk_worker::providers::ModelRequest, String)>>,
}

/// Keep the real task and runtime continuation intact. A targeted label probe
/// alone cannot show that the model can escape a repeated-selection loop.
#[tokio::test]
#[ignore = "one local inference; WW_REPLAY_REQUEST and WW_REPLAY_CONFIG; never executes tools"]
async fn saved_browser_repetition_decision() {
    use futures_util::StreamExt;
    use serde_json::Value;
    use werk_worker::providers::*;
    let request: ModelRequest = serde_json::from_slice(
        &std::fs::read(std::env::var("WW_REPLAY_REQUEST").expect("WW_REPLAY_REQUEST")).unwrap(),
    )
    .unwrap();
    let warning: Value = request
        .messages
        .iter()
        .filter_map(|m| serde_json::from_str::<Value>(&m.content).ok())
        .find(|v| v["event"] == "browser_repetition" || v["event"] == "browser_strategy")
        .expect("concrete repetition warning");
    let observation: Value = request
        .messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| serde_json::from_str::<Value>(&m.content).ok())
        .find(|v| !v["data"]["controls"].is_null())
        .expect("current controls");
    let controls = expanded_controls(&observation["data"]["controls"]);
    let config: WerkConfig = serde_json::from_slice(
        &std::fs::read(std::env::var("WW_REPLAY_CONFIG").expect("WW_REPLAY_CONFIG")).unwrap(),
    )
    .unwrap();
    let provider = WerkProvider::connect(config, std::env::var("WW_REPLAY_API_KEY").ok())
        .await
        .unwrap();
    let counted = provider.estimate_input_tokens(&request).await.unwrap();
    assert!(
        counted + request.max_output_tokens + 512
            <= provider.capabilities().context_tokens.unwrap()
    );
    let schemas = request.tools.clone();
    let mut stream = provider.stream(request).await.unwrap();
    let mut assembler = ResponseAssembler::default();
    while let Some(delta) = tokio::time::timeout(std::time::Duration::from_secs(180), stream.next())
        .await
        .unwrap()
    {
        assembler.push(&delta.unwrap(), 1_048_576).unwrap();
    }
    let (message, usage, _) = assembler.finish().unwrap();
    eprintln!(
        "unmodified task decision: calls={:?}, usage={usage:?}",
        message.tool_calls
    );
    if let Ok(path) = std::env::var("WW_REPLAY_DECISION") {
        std::fs::write(path, serde_json::to_vec(&message).unwrap()).unwrap();
    }
    assert!(
        !message.tool_calls.is_empty(),
        "must continue the pending task"
    );
    for call in &message.tool_calls {
        assert_eq!(
            call.name, "browser.action",
            "probe expects an actual task operation"
        );
        let schema = &schemas
            .iter()
            .find(|s| s.name == call.name)
            .unwrap()
            .input_schema;
        tools::validate_arguments(schema, &call.arguments).unwrap();
        assert_eq!(
            call.arguments["browser_id"],
            observation["data"]["browser_id"]
        );
        if let Some(reference) = call.arguments["target"]["ref"].as_str() {
            let selected = controls
                .iter()
                .find(|c| c["ref"] == reference)
                .expect("fresh observed ref");
            if warning["event"] == "browser_strategy" {
                assert!(
                    warning["data"]["untried_control_names"]
                        .as_array()
                        .unwrap()
                        .contains(&selected["name"]),
                    "strategy recovery must inspect a concrete alternative, not repeat setup"
                );
            }
            if call.arguments["action"] == warning["data"]["action"] {
                assert_ne!(
                    selected["bounds"], warning["data"]["target"]["bounds"],
                    "repeated warned control"
                );
            }
        }
        if call.arguments["action"] == "click_at" {
            if warning["event"] == "browser_strategy" {
                let x = call.arguments["x"].as_f64().unwrap();
                let y = call.arguments["y"].as_f64().unwrap();
                assert!(
                    controls.iter().any(|c| {
                        let b = &c["bounds"];
                        warning["data"]["untried_control_names"]
                            .as_array()
                            .unwrap()
                            .contains(&c["name"])
                            && x >= b["x"].as_f64().unwrap()
                            && x < b["x"].as_f64().unwrap() + b["width"].as_f64().unwrap()
                            && y >= b["y"].as_f64().unwrap()
                            && y < b["y"].as_f64().unwrap() + b["height"].as_f64().unwrap()
                    }),
                    "strategy recovery must select a new observed operation, not repeat a coordinate fill"
                );
            }
            let bounds = &warning["data"]["target"]["bounds"];
            if let (Some(x), Some(y), Some(bx), Some(by), Some(w), Some(h)) = (
                call.arguments["x"].as_f64(),
                call.arguments["y"].as_f64(),
                bounds["x"].as_f64(),
                bounds["y"].as_f64(),
                bounds["width"].as_f64(),
                bounds["height"].as_f64(),
            ) {
                assert!(
                    !(x >= bx && x < bx + w && y >= by && y < by + h),
                    "coordinate retry of warned control"
                );
            }
        }
    }
}

#[tokio::test]
#[ignore = "one local inference; WW_REPLAY_REQUEST, WW_REPLAY_CONFIG, WW_REPLAY_TARGET; never executes tools"]
async fn saved_browser_target_decision() {
    use futures_util::StreamExt;
    use werk_worker::providers::*;
    let mut request: ModelRequest = serde_json::from_slice(
        &std::fs::read(std::env::var("WW_REPLAY_REQUEST").expect("WW_REPLAY_REQUEST")).unwrap(),
    )
    .unwrap();
    let observation = request
        .messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| serde_json::from_str::<serde_json::Value>(&m.content).ok())
        .find(|v| v["data"]["controls"].is_object())
        .expect("projected control table");
    let label = std::env::var("WW_REPLAY_TARGET").expect("WW_REPLAY_TARGET");
    let controls = expanded_controls(&observation["data"]["controls"]);
    let targets: Vec<_> = controls.iter().filter(|c| c["name"] == label).collect();
    assert_eq!(targets.len(), 1, "probe requires a unique visible name");
    let reference = targets[0]["ref"].clone();
    let last = request.messages.last_mut().unwrap();
    assert_eq!(last.role, Role::User);
    last.content = format!(
        "For this diagnostic decision, select the visible {label:?} control. Return one browser.action click with its current target.ref and browser_id, using the observation. Keep narration brief."
    );
    request.max_output_tokens = 1024;
    let config: WerkConfig = serde_json::from_slice(
        &std::fs::read(
            std::env::var("WW_REPLAY_CONFIG")
                .expect("WW_REPLAY_CONFIG with confirmed tool capabilities"),
        )
        .unwrap(),
    )
    .unwrap();
    let provider = WerkProvider::connect(config, std::env::var("WW_REPLAY_API_KEY").ok())
        .await
        .unwrap();
    let counted = provider.estimate_input_tokens(&request).await.unwrap();
    assert!(
        counted + request.max_output_tokens + 512
            <= provider.capabilities().context_tokens.unwrap()
    );
    let action_schema = request
        .tools
        .iter()
        .find(|t| t.name == "browser.action")
        .unwrap()
        .input_schema
        .clone();
    let mut stream = provider.stream(request).await.unwrap();
    let mut assembler = ResponseAssembler::default();
    while let Some(delta) = tokio::time::timeout(std::time::Duration::from_secs(180), stream.next())
        .await
        .unwrap()
    {
        assembler.push(&delta.unwrap(), 1_048_576).unwrap();
    }
    let (message, usage, _) = assembler.finish().unwrap();
    eprintln!(
        "target decision: calls={:?}, usage={usage:?}",
        message.tool_calls
    );
    assert_eq!(message.tool_calls.len(), 1);
    let call = &message.tool_calls[0];
    assert_eq!(call.name, "browser.action");
    tools::validate_arguments(&action_schema, &call.arguments).unwrap();
    assert_eq!(call.arguments["action"], "click");
    assert_eq!(
        call.arguments["browser_id"],
        observation["data"]["browser_id"]
    );
    assert_eq!(call.arguments["target"]["ref"], reference);
}

#[async_trait::async_trait]
impl werk_worker::providers::ModelProvider for AdmissionCapture {
    fn capabilities(&self) -> werk_worker::providers::ModelCapabilities {
        self.provider.capabilities()
    }
    fn identity(&self) -> String {
        self.provider.identity()
    }
    fn max_output_tokens(&self) -> Option<usize> {
        self.provider.max_output_tokens()
    }
    async fn reasoning_options(
        &self,
        output: usize,
    ) -> Result<Option<werk_worker::providers::ReasoningOptions>> {
        self.provider.reasoning_options(output).await
    }
    async fn estimate_input_tokens(
        &self,
        request: &werk_worker::providers::ModelRequest,
    ) -> Result<usize> {
        let count = self.provider.estimate_input_tokens(request).await?;
        if request.messages.iter().any(|m| !m.images.is_empty()) {
            let mut best = self.best.lock().unwrap();
            if best.as_ref().is_none_or(|(old, _, _)| count < *old) {
                *best = Some((
                    count,
                    request.clone(),
                    self.provider.input_token_source(request).into(),
                ));
            }
        }
        Ok(count)
    }
    async fn stream(
        &self,
        _: werk_worker::providers::ModelRequest,
    ) -> Result<werk_worker::providers::ModelStream> {
        panic!("admission replay never generates")
    }
}

/// Uses only the local admission/token-count API, never generation or tools.
#[tokio::test]
#[ignore = "set WW_REPLAY_SNAPSHOT or WW_REPLAY_DATABASE/WW_REPLAY_SESSION; WW_REPLAY_CONFIG (or WW_REPLAY_MODEL); requires local Werk token counter"]
async fn saved_browser_admission_replay() {
    use werk_worker::{providers::*, tools::ToolRegistry};
    let text = if let Ok(path) = std::env::var("WW_REPLAY_SNAPSHOT") {
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
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    };
    let session: SessionData = serde_json::from_str(&text).unwrap();
    let original = fingerprint(&session);
    let run = session.runs.last().unwrap();
    let registry = ToolRegistry::builtins().unwrap();
    // Match the running worker's capabilities and wire settings. Model-only
    // defaults need not reproduce its offered catalog or reasoning budget.
    let config = if let Ok(path) = std::env::var("WW_REPLAY_CONFIG") {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    } else {
        WerkConfig::local(
            std::env::var("WW_REPLAY_MODEL").expect("WW_REPLAY_CONFIG or WW_REPLAY_MODEL"),
        )
    };
    let provider = WerkProvider::connect(config, std::env::var("WW_REPLAY_API_KEY").ok())
        .await
        .unwrap();
    let tools = context::request_tools(
        &session,
        run,
        &registry,
        &provider,
        &parallel::ExecutionConfig::default(),
    )
    .unwrap();
    let provider = AdmissionCapture {
        provider,
        best: Default::default(),
    };
    let built = DefaultContextBuilder
        .build_with_provider(&session, run, tools, &provider)
        .await;
    if let Some((count, request, source)) = provider.best.lock().unwrap().as_ref() {
        eprintln!(
            "lowest visual candidate: input={count}, output={}, source={source}, tool_bytes={}, message_bytes={:?}",
            request.max_output_tokens,
            serde_json::to_vec(&request.tools).unwrap().len(),
            request
                .messages
                .iter()
                .map(|m| (m.role.clone(), m.content.len()))
                .collect::<Vec<_>>()
        );
        if let Ok(path) = std::env::var("WW_REPLAY_CAPTURE") {
            std::fs::write(path, serde_json::to_vec(request).unwrap()).unwrap();
        }
    }
    let (request, snapshot) = built.unwrap();
    if let Ok(path) = std::env::var("WW_REPLAY_CAPTURE") {
        // On success capture the actual admitted output allowance, not an
        // earlier candidate with the same input count and a larger reserve.
        std::fs::write(path, serde_json::to_vec(&request).unwrap()).unwrap();
    }
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
    let current_observation = session
        .runs
        .iter()
        .flat_map(|r| &r.invocations)
        .rev()
        .find(|i| {
            browser_policy::TOOLS.contains(&i.call.name.as_str())
                && i.result
                    .as_ref()
                    .is_some_and(|r| r.data["observation_id"].is_string())
        })
        .unwrap();
    let receipt = current_observation.result.as_ref().unwrap();
    let retained = request
        .messages
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some(&current_observation.call.id))
        .expect("latest observation remains present, including after screenshot:false");
    assert_eq!(retained.images, receipt.images);
    let data: serde_json::Value = serde_json::from_str(&retained.content).unwrap();
    assert_eq!(
        data["data"]["observation_id"],
        receipt.data["observation_id"]
    );
    if let Some(controls) = receipt.data["controls"].as_array() {
        let actual = expanded_controls(&data["data"]["controls"]);
        for control in controls {
            let retained = actual.iter().find(|c| c["ref"] == control["ref"]).unwrap();
            assert_eq!(retained["bounds"], control["bounds"]);
            if control["name"]
                .as_str()
                .is_some_and(|name| !name.is_empty())
            {
                assert_eq!(retained["name"], control["name"]);
            }
        }
    }
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
