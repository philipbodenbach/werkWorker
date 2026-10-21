//! Run-bound adaptive behavior with explicit legacy snapshot migration. State uses the reserved metadata namespace
//! so existing public struct literals and schema-1 sessions remain compatible.
use crate::{
    core::*,
    providers::ModelRequest,
    tools::{validate_arguments, validate_schema},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const KEY: &str = "_werkworker_harness";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessMode {
    Classic,
    Minimal,
    #[default]
    Adaptive,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessConfig {
    /// Legacy compatibility input; new root runs always normalize to Adaptive.
    pub mode: HarnessMode,
    /// Explicit host-provided, model-specific guidance. Never sourced from tools.
    pub local_guidance: Option<String>,
    pub completion: Option<CompletionCheck>,
}

/// One explicitly configured tool, executed through the ordinary invocation path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionCheck {
    pub tool: String,
    pub arguments: Value,
    /// Validate ToolResult.data, in addition to successful status. For process.run
    /// require exit_code == 0 here; process success alone only means it ran.
    pub result_schema: Value,
    #[serde(default)]
    pub max_corrections: u32,
}
impl HarnessConfig {
    pub fn validate(&self) -> Result<()> {
        if self.local_guidance.as_ref().is_some_and(|s| s.len() > 8192) {
            return Err(WorkerError::new(
                "invalid_harness",
                "Local guidance exceeds 8192 bytes",
            ));
        }
        if let Some(check) = &self.completion {
            validate_id(&check.tool)?;
            validate_schema(&check.result_schema)?;
            if check.max_corrections > 3 || check.arguments.to_string().len() > 65536 {
                return Err(WorkerError::new(
                    "invalid_harness",
                    "Completion check exceeds bounded limits",
                ));
            }
        }
        Ok(())
    }
    /// Compatibility constructor. New roots migrate this to adaptive behavior.
    pub fn minimal() -> Self {
        Self {
            mode: HarnessMode::Minimal,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct HarnessState {
    #[serde(default = "legacy_version")]
    pub version: u32,
    pub config: HarnessConfig,
    #[serde(default)]
    pub adaptive: AdaptiveState,
    #[serde(default)]
    pub checks: Vec<CheckRecord>,
    #[serde(default)]
    pub messages: Vec<usize>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CheckRecord {
    pub candidate: String,
    pub invocation_id: String,
    pub verdict: Option<String>,
}

pub(crate) fn state(session: &SessionData, run_id: &str) -> Result<HarnessState> {
    match session.metadata.get(KEY).and_then(|v| v.get(run_id)) {
        Some(value) => {
            let mut value = value.clone();
            if value.get("version").and_then(Value::as_u64).unwrap_or(1) > 2 {
                return Err(WorkerError::new(
                    "harness_version",
                    "Unsupported harness state version",
                ));
            }
            // Omitted modes in schema-1 records meant classic, never adaptive.
            if let Some(config) = value.get_mut("config").and_then(Value::as_object_mut) {
                config.entry("mode").or_insert_with(|| json!("classic"));
            }
            Ok(serde_json::from_value(value)?)
        }
        None => Ok(HarnessState {
            config: HarnessConfig {
                mode: HarnessMode::Classic,
                ..Default::default()
            },
            ..Default::default()
        }),
    }
}
pub fn config(session: &SessionData, run: &Run) -> Result<HarnessConfig> {
    Ok(state(session, &run.id)?.config)
}
pub(crate) fn save(session: &mut SessionData, run_id: &str, state: &HarnessState) -> Result<()> {
    if !session.metadata.is_object() {
        let previous = std::mem::take(&mut session.metadata);
        session.metadata = if previous.is_null() {
            json!({})
        } else {
            json!({"_werkworker_legacy_metadata":previous})
        };
    }
    let metadata = session.metadata.as_object_mut().ok_or_else(|| {
        WorkerError::new("invalid_harness", "Harness metadata normalization failed")
    })?;
    let runs = metadata
        .entry(KEY)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            WorkerError::new(
                "invalid_harness",
                "Reserved harness metadata must be an object",
            )
        })?;
    runs.insert(run_id.into(), serde_json::to_value(state)?);
    Ok(())
}
pub(crate) fn bind(session: &mut SessionData, run_id: &str, config: HarnessConfig) -> Result<()> {
    config.validate()?;
    save(
        session,
        run_id,
        &HarnessState {
            version: 2,
            config,
            ..Default::default()
        },
    )
}

/// Use a supported low-authority role. Provenance is recorded separately from
/// model text; payloads cannot introduce system or user instructions.
pub(crate) fn feedback(kind: &str, data: Value) -> Message {
    Message::text(
        Role::Assistant,
        json!({"source":"werk_worker.harness", "event":kind, "data":data}).to_string(),
    )
}
pub(crate) fn push_feedback(
    session: &mut SessionData,
    run_id: &str,
    kind: &str,
    data: Value,
) -> Result<()> {
    let mut state = state(session, run_id)?;
    state.messages.push(session.messages.len());
    session.messages.push(feedback(kind, data));
    save(session, run_id, &state)
}

/// Count repeated calls since a changed observation, without trusting model text.
/// Adaptive and legacy minimal use this; classic keeps its workspace-revision rule.
pub(crate) fn repetitions(run: &Run, call: &ToolCall) -> usize {
    // Polling a proven active owned job is bounded by ordinary tool/step/time
    // limits; lack of a workspace write does not make waiting an error.
    if matches!(call.name.as_str(), "process.status" | "agents.status")
        && let Some(result) = run
            .invocations
            .iter()
            .rev()
            .filter(|i| i.call.name == call.name && i.call.arguments == call.arguments)
            .find_map(|i| i.result.as_ref())
        && result.status == Outcome::Succeeded
        && (result.data["status"] == "running"
            || result.data["agents"].as_array().is_some_and(|agents| {
                agents.iter().any(|a| {
                    matches!(
                        a["status"].as_str(),
                        Some("running" | "queued" | "waiting_input" | "waiting_approval")
                    )
                })
            }))
    {
        return 1;
    }
    let mut prior = std::collections::BTreeMap::new();
    let mut boundary = 0;
    for (index, invocation) in run.invocations.iter().enumerate() {
        let Some(result) = invocation
            .result
            .as_ref()
            .filter(|r| r.status == Outcome::Succeeded)
        else {
            continue;
        };
        let key = fingerprint(&(&invocation.call.name, &invocation.call.arguments));
        let observable = invocation
            .definition
            .as_ref()
            .is_some_and(|d| matches!(d.effect, Effect::Read | Effect::NetworkRead))
            || matches!(
                invocation.call.name.as_str(),
                "process.status" | "agents.status"
            );
        if observable {
            // Exclude elapsed time/output churn from job status evidence.
            let observation = if invocation.call.name == "process.status" {
                json!({"running":result.data["running"],"exit_code":result.data["exit_code"],"status":result.data["status"]})
            } else {
                result.data.clone()
            };
            let value = fingerprint(&observation);
            // A new query's first result is useful for research; repeated unchanged
            // reads cannot reset each other. Changed results start a new epoch.
            if prior.get(&key) != Some(&value) {
                boundary = index;
            }
            prior.insert(key, value);
        } else if (invocation.call.name != call.name || invocation.call.arguments != call.arguments)
            && invocation
                .definition
                .as_ref()
                .is_some_and(|d| matches!(d.effect, Effect::Write | Effect::Process))
        {
            boundary = index + 1;
        }
    }
    run.invocations[boundary..]
        .iter()
        .filter(|i| i.call.name == call.name && i.call.arguments == call.arguments)
        .count()
}
pub(crate) fn repeated_calls(session: &SessionData, run: &Run, call: &ToolCall) -> Result<usize> {
    Ok(if config(session, run)?.mode != HarnessMode::Classic {
        repetitions(run, call)
    } else {
        run.repeated_calls_without_progress(call)
    })
}
pub(crate) fn check_passes(check: &CompletionCheck, result: &ToolResult) -> bool {
    result.status == Outcome::Succeeded
        && !result.truncated
        && validate_arguments(&check.result_schema, &result.data).is_ok()
}

// Completion may be attempted again after a corrected candidate. Never use a
// write/submit/job-control action as a repeatable acceptance probe.
pub(crate) fn validate_check_tool(definition: &ToolDefinition) -> Result<()> {
    if matches!(definition.effect, Effect::Write | Effect::ExternalWrite)
        || (definition.effect == Effect::Process && definition.name != "process.run")
        || matches!(
            definition.name.as_str(),
            "delegate_task"
                | "agents.message"
                | "agents.cancel"
                | "process.cancel"
                | "interaction.ask"
                | "task.plan"
        )
    {
        return Err(WorkerError::new(
            "invalid_harness",
            "Completion checks require an observational tool or sandboxed process.run",
        ));
    }
    Ok(())
}

fn legacy_version() -> u32 {
    1
}

/// Migrate stored contracts, never reinterpret them using current defaults.
pub(crate) fn migrate(session: &mut SessionData) -> Result<()> {
    let ids: Vec<_> = session.runs.iter().map(|r| r.id.clone()).collect();
    for id in ids {
        let mut state = state(session, &id)?;
        if state.version != 2 {
            state.version = 2;
            save(session, &id, &state)?;
            diagnostic(session, &id, "migration", "legacy_contract_preserved");
        }
    }
    Ok(())
}

pub(crate) fn new_config(mut config: HarnessConfig) -> HarnessConfig {
    config.mode = HarnessMode::Adaptive;
    config
}

/// Five bounded aid slots, independent of model identity or technical capability.
/// A hint is delivered once per episode. Two successful model responses separate
/// episodes; at most three episodes per kind are allowed during one run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct AdaptiveState {
    invocations: usize,
    attempts: usize,
    contexts: usize,
    successes: usize,
    aids: std::collections::BTreeMap<String, Aid>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Aid {
    active: bool,
    trigger: String,
    scope: String,
    detail: String,
    episodes: usize,
    activated_at: usize,
    rearm_at: usize,
    delivered_step: Option<String>,
    reason: String,
}
fn diagnostic(session: &mut SessionData, run_id: &str, kind: &str, decision: &str) {
    session.emit(
        Some(run_id),
        EventKind::Diagnostic {
            code: format!("harness_{kind}_{decision}"),
        },
    );
}
fn activate(
    a: &mut AdaptiveState,
    kind: &str,
    trigger: &str,
    scope: &str,
    detail: &str,
    events: &mut Vec<(String, String)>,
) {
    if let Some(old) = a.aids.get(kind) {
        let reason = if old.active {
            Some("active")
        } else if a.successes < old.rearm_at {
            Some("cooldown")
        } else if old.scope == scope {
            Some("duplicate_scope")
        } else if old.episodes >= 3 {
            Some("episode_limit")
        } else {
            None
        };
        if let Some(reason) = reason {
            events.push((kind.into(), format!("suppressed_{reason}:{trigger}")));
            return;
        }
    }
    let episodes = a.aids.get(kind).map_or(1, |old| old.episodes + 1);
    a.aids.insert(
        kind.into(),
        Aid {
            active: true,
            trigger: trigger.into(),
            scope: scope.into(),
            detail: detail.into(),
            episodes,
            activated_at: a.successes,
            rearm_at: 0,
            delivered_step: None,
            reason: "observed".into(),
        },
    );
    events.push((kind.into(), format!("activated:{trigger}")));
}
fn resolve(a: &mut AdaptiveState, kind: &str, reason: &str, events: &mut Vec<(String, String)>) {
    if let Some(aid) = a.aids.get_mut(kind)
        && aid.active
    {
        aid.active = false;
        aid.rearm_at = a.successes + 2;
        aid.reason = reason.into();
        events.push((kind.into(), format!("deactivated_{reason}:{}", aid.trigger)));
    }
}
fn optional_source(call: &ToolCall) -> bool {
    call.name.starts_with("skills.")
        || call
            .arguments
            .get("path")
            .and_then(Value::as_str)
            .and_then(|p| std::path::Path::new(p).file_name())
            .and_then(|p| p.to_str())
            .is_some_and(|p| matches!(p, "AGENTS.md" | "SKILL.md"))
}

/// Observe durable receipts only; arbitrary model/tool prose cannot escalate control.
/// Called inside the ordinary session transaction before context planning.
pub(crate) fn refresh(session: &mut SessionData, run_id: &str) -> Result<()> {
    let mut state = state(session, run_id)?;
    if state.config.mode != HarnessMode::Adaptive {
        return Ok(());
    }
    let run = session.run(run_id)?;
    let a = &mut state.adaptive;
    let mut events = Vec::new();
    for attempt in run.attempts.iter().skip(a.attempts) {
        if attempt.completed {
            a.successes += 1;
            resolve(a, "output_limit", "complete_response", &mut events);
            let expired: Vec<_> = a
                .aids
                .iter()
                .filter(|(_, h)| h.active && a.successes >= h.activated_at + 2)
                .map(|(k, _)| k.clone())
                .collect();
            for kind in expired {
                resolve(a, &kind, "scope_expired", &mut events);
            }
        } else if let Some(error) = &attempt.error {
            if error.code == "model_output_limit" {
                activate(
                    a,
                    "output_limit",
                    &attempt.attempt_id,
                    "generation_limit",
                    "The limited generation was discarded; none of its calls ran. Retain prior successful work and use a smaller complete action.",
                    &mut events,
                );
            } else if error.code == "context_limit_updated" {
                activate(
                    a,
                    "context_pressure",
                    &attempt.attempt_id,
                    "server_limit",
                    "The verified context limit required rebuilding this request. Use retained receipts or targeted reads for omitted details.",
                    &mut events,
                );
            }
        }
    }
    a.attempts = run.attempts.len();
    let observed_invocation = a.invocations < run.invocations.len();
    for i in run.invocations.iter().skip(a.invocations) {
        let Some(result) = &i.result else {
            break;
        };
        a.invocations += 1;
        if result.status == Outcome::Succeeded {
            for kind in ["tool_validation", "missing_information"] {
                if a.aids
                    .get(kind)
                    .is_some_and(|h| h.scope.starts_with(&format!("{}:", i.call.name)))
                {
                    resolve(a, kind, "tool_succeeded", &mut events);
                }
            }
        }
        if result.truncated {
            activate(
                a,
                "output_limit",
                &i.invocation_id,
                &format!("{}:truncated", i.call.name),
                "This tool receipt reached its output limit. Use its artifact or a narrower query for needed details; do not repeat the same oversized read.",
                &mut events,
            );
        }
        let code = result.error_code.as_deref().unwrap_or("");
        let missing = matches!(code, "not_found" | "file_not_found" | "missing_information");
        if !optional_source(&i.call) && !missing && result.status == Outcome::ValidationError {
            activate(
                a,
                "tool_validation",
                &i.invocation_id,
                &format!("{}:{code}", i.call.name),
                "This tool rejected its arguments. Use its published schema and error receipt to correct only the rejected call.",
                &mut events,
            );
        }
        if !optional_source(&i.call) && missing {
            activate(
                a,
                "missing_information",
                &i.invocation_id,
                &format!("{}:{code}", i.call.name),
                "The requested information is unavailable. Inspect a relevant available source, or ask for the specific missing input using interaction.ask when available. Do not invent it.",
                &mut events,
            );
        }
    }
    if observed_invocation && let Some(i) = run.invocations.last() {
        let count = repetitions(run, &i.call);
        if count >= 2 {
            activate(
                a,
                "repeated_action",
                &i.invocation_id,
                &fingerprint(&(&i.call.name, &i.call.arguments)),
                "Repeated action produced no new evidence. Reuse its receipt or choose a different relevant action within policy.",
                &mut events,
            );
        } else if i
            .result
            .as_ref()
            .is_some_and(|r| r.status == Outcome::Succeeded)
        {
            resolve(a, "repeated_action", "new_evidence", &mut events);
        }
    }
    for context in run.contexts.iter().skip(a.contexts) {
        if !context.omitted_messages.is_empty() {
            activate(
                a,
                "context_pressure",
                &context.id,
                "compacted_history",
                "Context pressure omitted history. Retained evidence and artifacts remain available; retrieve only details needed for the next action.",
                &mut events,
            );
        } else {
            resolve(a, "context_pressure", "history_fits", &mut events);
        }
    }
    a.contexts = run.contexts.len();
    for (kind, decision) in events {
        diagnostic(session, run_id, &kind, &decision);
    }
    save(session, run_id, &state)
}

pub(crate) fn adaptive_feedback(session: &SessionData, run: &Run) -> Result<Vec<Message>> {
    let state = state(session, &run.id)?;
    if state.config.mode != HarnessMode::Adaptive {
        return Ok(Vec::new());
    }
    Ok(state
        .adaptive
        .aids
        .iter()
        .filter(|(_, aid)| {
            aid.active
                && aid.delivered_step.as_ref().is_none_or(|step| {
                    !run.attempts
                        .iter()
                        .any(|a| &a.step_id == step && (a.completed || a.error.is_some()))
                })
        })
        .map(|(kind, aid)| {
            feedback(
                kind,
                json!({"trigger":aid.trigger,"scope":aid.scope,"detail":aid.detail}),
            )
        })
        .collect())
}

/// Delivery is persisted with the exact planned request, before provider dispatch.
/// Resume can reconstruct a hint if dispatch never completed; transport retries
/// reuse the same request and do not activate another episode.
pub(crate) fn delivered(
    session: &mut SessionData,
    run_id: &str,
    step: &str,
    request: &ModelRequest,
) -> Result<()> {
    let mut state = state(session, run_id)?;
    if state.config.mode != HarnessMode::Adaptive {
        return Ok(());
    }
    let pending = adaptive_feedback(session, session.run(run_id)?)?;
    for hint in pending {
        if !request.messages.iter().any(|m| m.content == hint.content) {
            continue;
        }
        let value: Value = serde_json::from_str(&hint.content)?;
        let kind = value["event"].as_str().unwrap();
        state.adaptive.aids.get_mut(kind).unwrap().delivered_step = Some(step.into());
        diagnostic(session, run_id, kind, "delivered");
    }
    save(session, run_id, &state)
}

pub(crate) fn finish(session: &mut SessionData, run_id: &str) -> Result<()> {
    let mut state = state(session, run_id)?;
    if state.config.mode != HarnessMode::Adaptive {
        return Ok(());
    }
    let mut events = Vec::new();
    let kinds: Vec<_> = state.adaptive.aids.keys().cloned().collect();
    for kind in kinds {
        resolve(&mut state.adaptive, &kind, "run_finished", &mut events);
    }
    for (kind, decision) in events {
        diagnostic(session, run_id, &kind, &decision);
    }
    save(session, run_id, &state)
}
