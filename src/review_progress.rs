//! Inspect observed information rather than counting model decisions. Rebuilt
//! from receipts, so compaction/restart cannot replenish a stalled review.
use crate::{core::*, read_progress::Coverage};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn idle_decisions(run: &Run) -> usize {
    let mut coverage: BTreeMap<(String, String), Coverage> = BTreeMap::new();
    let mut observations = BTreeSet::new();
    let mut decisions = BTreeMap::<usize, bool>::new();
    for invocation in &run.invocations {
        let Some(result) = &invocation.result else {
            continue;
        };
        let fresh = decisions.entry(invocation.message_index).or_default();
        if result.status != Outcome::Succeeded {
            continue;
        }
        if invocation.call.name == "files.read"
            && let (Some(path), Some(hash), Some(start), Some(content)) = (
                result.data["path"].as_str(),
                result.data["sha256"].as_str(),
                result.data["start_byte"].as_u64(),
                result.data["content"].as_str(),
            )
        {
            let key = (
                crate::working_state::path(path).unwrap_or_else(|_| path.into()),
                hash.into(),
            );
            let first_version = !coverage.contains_key(&key);
            let bytes = coverage.entry(key).or_default().observe(
                hash,
                start,
                start.saturating_add(content.len() as u64),
            );
            *fresh |= first_version || bytes > 0;
            continue;
        }
        // Different queries/pagination over the same rows are not new facts.
        // Empty searches count once, so a finite number of useful negative
        // probes is allowed, without letting random query strings reset us.
        let rows = match invocation.call.name.as_str() {
            "files.find" => result.data["paths"].as_array(),
            "files.search" => result.data["matches"].as_array(),
            "files.list" => result.data["entries"].as_array(),
            _ => None,
        };
        if let Some(rows) = rows {
            for row in rows {
                *fresh |= observations.insert(fingerprint(&(invocation.call.name.as_str(), row)));
            }
            if rows.is_empty() {
                *fresh |=
                    observations.insert(fingerprint(&(invocation.call.name.as_str(), "empty")));
            }
        } else {
            let data = match invocation.call.name.as_str() {
                "agents.status" => json!({"agents":result.data["agents"]}),
                "process.status" => {
                    json!({"running":result.data["running"],"status":result.data["status"],"exit_code":result.data["exit_code"]})
                }
                _ => stable_data(&result.data),
            };
            *fresh |= observations.insert(fingerprint(&(invocation.call.name.as_str(), data)));
        }
    }
    decisions
        .values()
        .rev()
        .take_while(|fresh| !**fresh)
        .count()
}

fn stable_data(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "duration_ms"
                            | "elapsed_ms"
                            | "timestamp_ms"
                            | "request_id"
                            | "invocation_id"
                    )
                })
                .map(|(key, value)| (key.clone(), stable_data(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(stable_data).collect()),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run() -> Run {
        serde_json::from_value(json!({"id":"run", "status":"running", "input":"Review", "limits":Limits::continuous(), "started_at_ms":0, "deadline_ms":0,
            "history_start":0,"metrics":Metrics::default(),"attempts":[],"contexts":[],"invocations":[],"steering":[],"plan":null,"similar_grants":[],"repaired_output":false})).unwrap()
    }
    fn observation(run: &mut Run, decision: usize, tool: &str, args: Value, data: Value) {
        run.invocations.push(serde_json::from_value(json!({"invocation_id":format!("inv_{}",run.invocations.len()),"step_id":format!("step_{decision}"),"message_index":decision,
            "call":{"id":format!("call_{}",run.invocations.len()),"name":tool,"arguments":args},"status":"finished","prepared":{},"result":ToolResult::ok(data)})).unwrap());
    }
    #[test]
    fn overlapping_reads_and_alternating_observations_do_not_create_progress() {
        let mut run = run();
        for n in 0..12 {
            observation(
                &mut run,
                n,
                "files.read",
                json!({"path":"app.rs"}),
                json!({"path":"app.rs","sha256":"version","start_byte":n*4,"content":"code"}),
            );
            assert_eq!(idle_decisions(&run), 0);
        }
        for n in 12..17 {
            observation(
                &mut run,
                n,
                "files.read",
                json!({"path":"./app.rs","start_line":n}),
                json!({"path":"./app.rs","sha256":"version","start_byte":n%4,"content":"code"}),
            );
        }
        assert_eq!(idle_decisions(&run), 5);
        // A novel range in the same parallel decision resets the entire batch.
        observation(
            &mut run,
            16,
            "files.read",
            json!({}),
            json!({"path":"app.rs","sha256":"version","start_byte":100,"content":"new"}),
        );
        assert_eq!(idle_decisions(&run), 0);
        let restored: Run = serde_json::from_value(serde_json::to_value(&run).unwrap()).unwrap();
        assert_eq!(idle_decisions(&restored), 0);
    }
    #[test]
    fn query_churn_and_capacity_polling_do_not_reset_the_breaker() {
        let mut run = run();
        for n in 0..5 {
            observation(
                &mut run,
                n,
                "files.search",
                json!({"query":format!("guess{n}")}),
                json!({"matches":[],"offset":n}),
            );
        }
        assert_eq!(idle_decisions(&run), 4);
        for n in 5..10 {
            observation(
                &mut run,
                n,
                "agents.status",
                json!({}),
                json!({"agents":[],"execution":{"free_model_slots":n%2}}),
            );
        }
        assert_eq!(idle_decisions(&run), 4);
    }
}
