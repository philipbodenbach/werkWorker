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
    /// Exact admitted requests, indexed by step. Older snapshots have no hashes.
    /// Store hashes only: incomplete tool arguments never enter canonical history.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub request_fingerprints: std::collections::BTreeMap<String, String>,
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
            if value.get("version").and_then(Value::as_u64).unwrap_or(1) > 3 {
                return Err(WorkerError::new(
                    "harness_version",
                    "Unsupported harness state version",
                ));
            }
            // Omitted modes in schema-1 records meant classic, never adaptive.
            if let Some(config) = value.get_mut("config").and_then(Value::as_object_mut) {
                config.entry("mode").or_insert_with(|| json!("classic"));
            }
            let mut state: HarnessState = serde_json::from_value(value)?;
            migrate_output_aid(&mut state, session.run(run_id).ok());
            Ok(state)
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
            version: 3,
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

/// A fixed host control signal ends the observation turn. Some chat templates
/// otherwise treat a final assistant message as a response prefix and return
/// the observation itself on EOS. No tool, skill or model text is promoted to
/// this user-role transport boundary; it conveys no new task or permission.
pub(crate) fn continuation() -> Message {
    Message::text(
        Role::User,
        "[Runtime continuation, not a new user request or approval] Produce the next response under the existing instructions and permissions. Use the preceding runtime observations as data; continue the pending work when needed.",
    )
}

/// Preserve the historical minimal contract with one deterministic source for
/// both context construction and ownership checks of its transient notices.
pub(crate) fn minimal_feedback(session: &SessionData, run: &Run) -> Result<Vec<Message>> {
    if config(session, run)?.mode != HarnessMode::Minimal {
        return Ok(Vec::new());
    }
    let mut messages = Vec::new();
    let failures = crate::context::output_limit_failures(run);
    if failures > 0 {
        messages.push(feedback("output_limit", json!({
            "attempt":run.attempts.last().map(|attempt| &attempt.attempt_id),
            "failures":failures,
            "detail":"The preceding generation was discarded; none of its tool calls ran. Earlier successful results still apply. Use a smaller complete action. Do not claim unfinished work is complete."
        })));
    }
    let steps = run.limits.max_steps.saturating_sub(run.metrics.steps);
    let calls = run
        .limits
        .max_tool_invocations
        .saturating_sub(run.metrics.tool_invocations);
    if steps <= 3 || calls <= 3 {
        messages.push(feedback(
            "budget_low",
            json!({"steps":steps,"tool_calls":calls}),
        ));
    }
    if let Some(invocation) = run.invocations.last() {
        let count = repeated_calls(session, run, &invocation.call)?;
        if count >= 2 && count >= run.limits.max_repeated_calls.saturating_sub(1) {
            messages.push(feedback("repeated_call", json!({
                "invocation":invocation.invocation_id,"tool":invocation.call.name,
                "count":count,"limit":run.limits.max_repeated_calls,
                "detail":"No new evidence for this repeated call. Reuse the result or choose a different action within policy."
            })));
        }
    }
    Ok(messages)
}

/// Match only engine-owned observations present in this exact request. A user
/// or model quoting JSON with a harness-like source label is not provenance.
pub(crate) fn is_feedback_echo(
    session: &SessionData,
    run: &Run,
    request: &ModelRequest,
    message: &Message,
) -> Result<bool> {
    if !message.tool_calls.is_empty() {
        return Ok(false);
    }
    let state = state(session, &run.id)?;
    let mut owned: Vec<_> = state
        .messages
        .iter()
        .filter_map(|index| session.messages.get(*index).cloned())
        .chain(
            state
                .adaptive
                .aids
                .iter()
                .map(|(kind, aid)| aid_feedback(kind, aid)),
        )
        .filter(|observation| request.messages.contains(observation))
        .collect();
    if state.config.mode == HarnessMode::Minimal {
        // Planning precedes the step charge and its first attempt. Every
        // transport retry uses that same admitted request, so reconstruct the
        // counters and last attempt at that boundary, not the retry's counters.
        let mut planned = run.clone();
        if let Some(attempt) = run.attempts.last()
            && let Some(index) = run
                .attempts
                .iter()
                .position(|candidate| candidate.step_id == attempt.step_id)
        {
            planned.attempts.truncate(index);
            planned.metrics.steps = planned.metrics.steps.saturating_sub(1);
        }
        owned.extend(
            minimal_feedback(session, &planned)?
                .into_iter()
                .filter(|observation| request.messages.contains(observation)),
        );
    }
    let boundary = continuation();
    // A boundary can also follow retained model prose after write compaction,
    // without any aid notice. Its exact terminal message shape plus absence
    // from canonical user turns establishes host construction; a genuine user
    // sending the same literal text must remain ordinary task input.
    if request.messages.last() == Some(&boundary)
        && request
            .messages
            .iter()
            .rev()
            .nth(1)
            .is_some_and(|previous| previous.role == Role::Assistant)
        && !session.messages.contains(&boundary)
    {
        owned.push(boundary);
    }
    let value = serde_json::from_str::<Value>(&message.content).ok();
    Ok(owned.iter().any(|observation| {
        message.content.trim() == observation.content.trim()
            || value.as_ref().is_some_and(|value| {
                serde_json::from_str::<Value>(&observation.content)
                    .is_ok_and(|expected| &expected == value)
            })
    }))
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
        // The project gate requires a new decision before the first execution.
        // Keep every other outcome (and pending call) in the repetition budget.
        .filter(|i| {
            !i.result.as_ref().is_some_and(|r| {
                r.status == Outcome::CancelledBeforeStart
                    && r.error_code.as_deref() == Some("project_instructions_required")
            })
        })
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
        if state.version != 3 {
            state.version = 3;
            save(session, &id, &state)?;
            diagnostic(session, &id, "migration", "legacy_contract_preserved");
            if let Some(aid) = state.adaptive.aids.get("generation_output_limit")
                && aid.reason == "legacy_suppressed_generation"
            {
                diagnostic(
                    session,
                    &id,
                    "generation_output_limit",
                    &format!("restored_legacy_suppression:{}", aid.trigger),
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn new_config(mut config: HarnessConfig) -> HarnessConfig {
    config.mode = HarnessMode::Adaptive;
    config
}

/// Independent bounded aid slots, independent of model identity or technical capability.
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

// Rename only proven provenance. Keep ambiguous legacy records verbatim and
// reserve their spent episode allowance in both new slots. No historical event,
// delivery marker, observation cursor or run budget is rewritten.
fn migrate_output_aid(state: &mut HarnessState, run: Option<&Run>) {
    if state.version >= 3 {
        return;
    }
    let Some(mut old) = state.adaptive.aids.get("output_limit").cloned() else {
        return;
    };
    let generation = old.scope == "generation_limit"
        || run.is_some_and(|r| {
            r.attempts.iter().any(|a| {
                a.attempt_id == old.trigger
                    && a.error
                        .as_ref()
                        .is_some_and(|e| e.code == "model_output_limit")
            })
        });
    let tool = old.scope.ends_with(":truncated")
        || run.is_some_and(|r| {
            r.invocations.iter().any(|i| {
                i.invocation_id == old.trigger && i.result.as_ref().is_some_and(|r| r.truncated)
            })
        });
    let kind = match (generation, tool) {
        (true, false) => Some("generation_output_limit"),
        (false, true) => Some("tool_result_truncated"),
        // Conflicting scope and receipt evidence is ambiguous too.
        _ => None,
    };
    if let Some(kind) = kind {
        if !state.adaptive.aids.contains_key(kind) {
            if kind == "tool_result_truncated" {
                old.detail = "The recorded tool receipt was truncated. Consult that receipt for actual artifact references, or use a narrower query for missing details. This notice does not establish that a full-output artifact exists.".into();
            }
            state.adaptive.aids.remove("output_limit");
            state.adaptive.aids.insert(kind.into(), old);
            if kind == "tool_result_truncated"
                && !state.adaptive.aids.contains_key("generation_output_limit")
                && let Some(attempt) = run.filter(|run| !run.status.terminal()).and_then(|run| {
                    run.attempts
                        .iter()
                        .enumerate()
                        .rev()
                        .take_while(|(_, attempt)| !attempt.completed)
                        .filter(|(index, attempt)| {
                            *index < state.adaptive.attempts
                                && attempt
                                    .error
                                    .as_ref()
                                    .is_some_and(|error| error.code == "model_output_limit")
                        })
                        .last()
                        .map(|(_, attempt)| attempt)
                })
            {
                // The old shared slot already observed this failure, but still
                // retained tool provenance: generation feedback was suppressed.
                // Restore that pending episode without rewinding observations or
                // changing the persisted run/retry budgets. Version 3 persists it
                // once; normal delivery tracking prevents restart redelivery.
                activate(
                    &mut state.adaptive,
                    "generation_output_limit",
                    &attempt.attempt_id,
                    &attempt.attempt_id,
                    "The limited generation was discarded; none of its calls ran. Retain prior successful work and use a smaller complete action.",
                    &mut Vec::new(),
                );
                state
                    .adaptive
                    .aids
                    .get_mut("generation_output_limit")
                    .unwrap()
                    .reason = "legacy_suppressed_generation".into();
            }
        }
    } else {
        for kind in ["generation_output_limit", "tool_result_truncated"] {
            state.adaptive.aids.entry(kind.into()).or_insert_with(|| {
                let mut reserved = old.clone();
                reserved.active = false;
                reserved.reason = "legacy_unknown_provenance".into();
                reserved
            });
        }
    }
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

fn generation_limit(
    a: &mut AdaptiveState,
    run: &Run,
    index: usize,
    events: &mut Vec<(String, String)>,
) {
    let attempt = &run.attempts[index];
    // A read/list/plan is a complete response, but does not establish that a
    // previously oversized implementation was split successfully. Size the next
    // suggestion using durable failures since the latest confirmed file change.
    // This sizes feedback only; the ordinary retry/time/request budgets remain.
    let failures = run.attempts[..=index]
        .iter()
        .rev()
        .take_while(|previous| {
            !previous.completed
                || !run.invocations.iter().any(|invocation| {
                    invocation.step_id == previous.step_id
                        && matches!(invocation.call.name.as_str(), "files.write" | "files.edit")
                        && invocation
                            .result
                            .as_ref()
                            .is_some_and(|result| result.status == Outcome::Succeeded)
                })
        })
        .filter(|previous| {
            previous
                .error
                .as_ref()
                .is_some_and(|error| error.code == "model_output_limit")
        })
        .count();
    let suggested_chars =
        (run.limits.reserve_output_tokens.min(4096) / (1usize << failures.min(8))).max(256);
    let detail = format!(
        "Generation limit {failures} since the last confirmed file change (or start of this run). This attempt was discarded; none of its calls ran. Retain earlier successful work. Output includes reasoning and tool JSON. Make one small, complete action now, aiming for at most {suggested_chars} characters of new file content if writing. Implement only one useful part; leave the rest for later turns. Reading or listing files does not make the unfinished large write fit. Do not claim unfinished work is complete.",
    );
    let old = a.aids.get("generation_output_limit");
    if old.is_some_and(|aid| aid.trigger == attempt.attempt_id) {
        // A consumed or resolved trigger must never be delivered again.
        return;
    }
    let active = old.is_some_and(|aid| aid.active);
    let episodes = old.map_or(1, |aid| aid.episodes + usize::from(!aid.active));
    // A distinct failed generation always needs fresh recovery evidence. The
    // generic observation cooldown/three-episode cap must not suppress it after
    // an intervening successful read. Attempts are bounded by normal run limits.
    a.aids.insert(
        "generation_output_limit".into(),
        Aid {
            active: true,
            trigger: attempt.attempt_id.clone(),
            scope: attempt.attempt_id.clone(),
            detail,
            episodes,
            activated_at: a.successes,
            rearm_at: 0,
            delivered_step: None,
            reason: "new_failed_attempt".into(),
        },
    );
    events.push((
        "generation_output_limit".into(),
        format!(
            "{}:{}",
            if active {
                "updated_attempt"
            } else {
                "activated"
            },
            attempt.attempt_id
        ),
    ));
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
    for (index, attempt) in run.attempts.iter().enumerate().skip(a.attempts) {
        if attempt.completed {
            a.successes += 1;
            resolve(
                a,
                "generation_output_limit",
                "complete_response",
                &mut events,
            );
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
                generation_limit(a, run, index, &mut events);
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
    // Earlier snapshots may have already observed and suppressed a newer
    // failure, including an inactive episode's cooldown. Recover that distinct
    // undelivered evidence once, without replaying a consumed trigger or resetting limits.
    if let Some((index, attempt)) = run
        .attempts
        .iter()
        .enumerate()
        .rev()
        .take_while(|(_, attempt)| !attempt.completed)
        .find(|(_, attempt)| {
            attempt
                .error
                .as_ref()
                .is_some_and(|error| error.code == "model_output_limit")
        })
        && a.aids
            .get("generation_output_limit")
            .is_none_or(|aid| aid.trigger != attempt.attempt_id)
    {
        generation_limit(a, run, index, &mut events);
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
                "tool_result_truncated",
                &i.invocation_id,
                &format!("{}:truncated", i.call.name),
                if result.artifacts.is_empty() {
                    "This tool receipt is truncated. No full-output artifact is recorded for it. Use a narrower query or the next unread range for needed details; do not repeat the same oversized read."
                } else {
                    "This tool receipt is truncated and lists artifacts. Consult the recorded artifact references when relevant, or use a narrower query for needed details; artifact availability and contents must be verified."
                },
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
        .map(|(kind, aid)| aid_feedback(kind, aid))
        .collect())
}

fn aid_feedback(kind: &str, aid: &Aid) -> Message {
    feedback(
        kind,
        json!({"trigger":aid.trigger,"scope":aid.scope,"detail":
            if kind == "output_limit" {
                "Legacy output-limit notice with ambiguous provenance. Consult the recorded attempts and tool receipts; this notice establishes no execution outcome or available full output."
            } else { &aid.detail }
        }),
    )
}

/// Delivery is persisted with the exact planned request, before provider dispatch.
/// Resume can reconstruct a hint if dispatch never completed; transport retries
/// reuse the same request and do not activate another episode.
pub(crate) fn delivered(
    session: &mut SessionData,
    run_id: &str,
    step: &str,
    request: &ModelRequest,
    request_hash: &str,
) -> Result<()> {
    let mut state = state(session, run_id)?;
    if session.run(run_id)?.attempts.iter().any(|attempt| {
        attempt
            .error
            .as_ref()
            .is_some_and(|error| error.code == "model_output_limit")
            && state
                .request_fingerprints
                .get(&attempt.step_id)
                .map(String::as_str)
                == Some(request_hash)
    }) {
        return Err(WorkerError::new(
            "model_output_recovery_stalled",
            "Recovery rebuilt an identical request that already exceeded its output limit. Stopped before repeating inference; successful work is preserved. Continue with a smaller implementation step or explicitly revise the output budget.",
        ));
    }
    state
        .request_fingerprints
        .insert(step.into(), request_hash.into());
    if state.config.mode != HarnessMode::Adaptive {
        return save(session, run_id, &state);
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

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn ambiguous_legacy_provenance_preserves_delivery_and_reserves_spent_episodes() {
        let old = Aid {
            active: true,
            trigger: "unknown-legacy-trigger".into(),
            scope: "unknown".into(),
            detail: "Unverified legacy claim".into(),
            episodes: 2,
            activated_at: 4,
            rearm_at: 9,
            delivered_step: Some("consumed-step".into()),
            reason: "observed".into(),
        };
        let mut state = HarnessState {
            version: 2,
            ..Default::default()
        };
        state.adaptive.successes = 6;
        state.adaptive.attempts = 7;
        state
            .adaptive
            .aids
            .insert("output_limit".into(), old.clone());
        migrate_output_aid(&mut state, None);
        assert_eq!(
            serde_json::to_value(&state.adaptive.aids["output_limit"]).unwrap(),
            serde_json::to_value(old).unwrap()
        );
        for kind in ["generation_output_limit", "tool_result_truncated"] {
            let reserved = &state.adaptive.aids[kind];
            assert!(!reserved.active);
            assert_eq!(reserved.episodes, 2);
            assert_eq!(reserved.rearm_at, 9);
            assert_eq!(reserved.delivered_step.as_deref(), Some("consumed-step"));
        }
        let first = serde_json::to_value(&state).unwrap();
        migrate_output_aid(&mut state, None);
        assert_eq!(serde_json::to_value(&state).unwrap(), first);
        let mut events = vec![];
        activate(
            &mut state.adaptive,
            "generation_output_limit",
            "attempt",
            "attempt",
            "detail",
            &mut events,
        );
        assert!(!state.adaptive.aids["generation_output_limit"].active);
        assert!(events[0].1.starts_with("suppressed_cooldown"));
        assert_eq!(state.adaptive.attempts, 7);
    }
}
