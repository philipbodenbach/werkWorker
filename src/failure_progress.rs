//! Repeated command failures are independent of intervening searches, reads,
//! model diagnoses and compaction. Only observed file changes rearm this guard.
use crate::core::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) struct FailureProgress {
    pub invocation_id: String,
    pub occurrences: usize,
    pub exit_code: i64,
    pub command: Value,
    pub detail: String,
}

pub(crate) fn enabled(session: &SessionData, run: &Run) -> Result<bool> {
    Ok(
        crate::harness::config(session, run)?.mode == crate::harness::HarnessMode::Adaptive
            && crate::completion::enabled(session, run)?,
    )
}

pub(crate) fn stop_limit(run: &Run) -> usize {
    run.limits.max_repeated_calls.max(1).saturating_add(2)
}

pub(crate) fn latest(run: &Run) -> Option<FailureProgress> {
    let mut files = BTreeMap::<String, String>::new();
    let mut failures = BTreeMap::<String, usize>::new();
    let mut latest = None;
    for invocation in &run.invocations {
        let Some(result) = invocation
            .result
            .as_ref()
            .filter(|r| r.status == Outcome::Succeeded)
        else {
            continue;
        };
        let mut changed = result.data["workspace_changed"] == true;
        if let (Some(path), Some(hash)) = (
            result.data["path"]
                .as_str()
                .and_then(|p| crate::working_state::path(p).ok()),
            result.data["sha256"].as_str(),
        ) {
            let previous = files.insert(path, hash.into());
            let previous = previous
                .as_deref()
                .or_else(|| invocation.call.arguments["expected_hash"].as_str());
            changed |= previous.is_some_and(|old| old != hash)
                || (previous.is_none()
                    && matches!(
                        invocation.call.name.as_str(),
                        "files.write" | "files.edit" | "files.insert"
                    ));
        }
        // A check may observe an external change before any subsequent read.
        if let Some(observed) = result.data["verification"]["files"].as_object() {
            for (path, hash) in observed {
                if let (Ok(path), Some(hash)) = (crate::working_state::path(path), hash.as_str()) {
                    changed |= files
                        .insert(path, hash.into())
                        .is_some_and(|old| old != hash);
                }
            }
        }
        if changed {
            failures.clear();
            latest = None;
        }
        if invocation.call.name != "process.run" || result.truncated {
            continue;
        }
        if result.data["exit_code"] == 0 && result.data["verification"]["passed"] == true {
            // Do not carry a stale warning into a successful final report.
            // Keep the counts: an unrelated passing check cannot replenish them.
            latest = None;
        }
        let Some(exit_code) = result.data["exit_code"].as_i64().filter(|code| *code != 0) else {
            continue;
        };
        let stdout = result.data["stdout"].as_str().unwrap_or("");
        let stderr = result.data["stderr"].as_str().unwrap_or("");
        if stdout.is_empty() && stderr.is_empty() && result.data["verification"]["passed"] != false
        {
            // Silent exit codes (e.g. a search without matches) alone are not
            // enough to identify the same failed behavioral check.
            continue;
        }
        let cwd =
            crate::working_state::path(invocation.call.arguments["cwd"].as_str().unwrap_or("."))
                .ok();
        // Match observed outcomes, not model-authored criteria, command spelling
        // or volatile process timings. Alternating queries cannot erase these.
        let key = fingerprint(&(cwd, exit_code, stdout, stderr));
        let occurrences = failures.entry(key).or_default();
        *occurrences += 1;
        latest = Some(FailureProgress {
            invocation_id: invocation.invocation_id.clone(),
            occurrences: *occurrences,
            exit_code,
            command: invocation.call.arguments["argv"].clone(),
            detail: if stderr.is_empty() { stdout } else { stderr }.into(),
        });
    }
    latest
}

pub(crate) fn feedback(session: &SessionData, run: &Run) -> Result<Option<Message>> {
    if !enabled(session, run)? {
        return Ok(None);
    }
    Ok(latest(run)
        .filter(|p| p.occurrences >= run.limits.max_repeated_calls.max(1))
        .map(|p| {
            crate::harness::feedback(
                "check_progress",
                json!({
                    "source_invocation":p.invocation_id,
                    "same_failure_occurrences":p.occurrences,
                    "stop_after_occurrences":stop_limit(run),
                    "remaining_attempts":stop_limit(run).saturating_sub(p.occurrences),
                    "last_command":p.command,"exit_code":p.exit_code,
                    "failure_excerpt":crate::completion::excerpt(&p.detail, 1000),
                    "detail":"The same command failure keeps returning without an observed file change. Reuse this diagnosis and make a targeted correction, or choose a diagnostic that tests a different hypothesis. Repeating searches, reading new ranges, renaming criteria or restating the diagnosis does not reset this failure counter. The circuit breaker stops the run at stop_after_occurrences, including in continuous mode. Existing work is retained; recovery is not completion."
                }),
            )
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "explicit read-only replay of a saved session; no model or workspace execution"]
    fn saved_session_stops_repeated_failure_before_endless_diagnosis() {
        let path = std::env::var("WW_REPLAY_DATABASE").expect("WW_REPLAY_DATABASE");
        let id = std::env::var("WW_REPLAY_SESSION").expect("WW_REPLAY_SESSION");
        let db =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let text: String = db
            .query_row("SELECT data FROM sessions WHERE id = ?1", [&id], |r| {
                r.get(0)
            })
            .unwrap();
        let session: SessionData = serde_json::from_str(&text).unwrap();
        let original = session.runs.last().unwrap();
        let mut run = original.clone();
        let mut stopped = None;
        for n in 1..=original.invocations.len() {
            run.invocations = original.invocations[..n].to_vec();
            if let Some(progress) = latest(&run).filter(|p| p.occurrences >= stop_limit(&run)) {
                stopped = Some(n);
                eprintln!(
                    "offline failure replay: stopped at tool {n}/{} after {} identical failures; {}",
                    original.invocations.len(),
                    progress.occurrences,
                    crate::completion::excerpt(&progress.detail, 300)
                );
                break;
            }
        }
        assert!(stopped.is_some_and(|n| n <= original.invocations.len() / 2 + 1));
    }
}
