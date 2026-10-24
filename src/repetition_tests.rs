//! Count equivalence and local CPU measurements; never invokes an inference server.
use crate::{core::*, harness, working_state};
use serde_json::json;
use std::{collections::BTreeMap, time::Instant};

fn run() -> Run {
    Run {
        id: "count-fixture".into(),
        status: RunStatus::Running,
        input: "fixture".into(),
        tree_model_requests: 0,
        children: vec![],
        input_images: vec![],
        limits: Limits::default(),
        output_schema: None,
        started_at_ms: 0,
        deadline_ms: 0,
        approval_wait_started_at_ms: None,
        generation_recovery: GenerationRecovery::default(),
        history_start: 0,
        final_text: None,
        error: None,
        metrics: Metrics::default(),
        attempts: vec![],
        contexts: vec![],
        invocations: vec![],
        steering: vec![],
        steering_messages: vec![],
        steering_context_pending: false,
        plan: serde_json::Value::Null,
        similar_grants: vec![],
        repaired_output: false,
    }
}

fn read(n: usize) -> ToolInvocation {
    let path = ["file.js", "./file.js", "/workspace/file.js"][n % 3];
    ToolInvocation {
        request_input_schema: None,
        invocation_id: format!("read-{n}"),
        step_id: "step".into(),
        message_index: n * 2,
        call: ToolCall {
            id: format!("call-{n}"),
            name: "files.read".into(),
            arguments: json!({"path":path,"start_line":n % 128}),
        },
        definition: Some(ToolDefinition {
            name: "files.read".into(),
            version: "1".into(),
            description: "fixture".into(),
            input_schema: json!({}),
            effect: Effect::Read,
            timeout_ms: 1000,
            output_limit: 8192,
            concurrency_safe: true,
        }),
        status: InvocationStatus::Finished,
        prepared: json!({}),
        result: Some(ToolResult::ok(
            json!({"path":"file.js","sha256":"version-a",
            "content":"observed source line\n".repeat(4)}),
        )),
        approval: None,
        interaction: None,
    }
}

fn old_counts(run: &Run, calls: &[ToolCall]) -> Vec<usize> {
    calls
        .iter()
        .map(|call| harness::repetitions(run, call))
        .collect()
}
fn indexed_counts(run: &Run, calls: &[ToolCall]) -> Vec<usize> {
    let counts = harness::repetition_counts(run);
    calls
        .iter()
        .map(|call| *counts.get(&working_state::call_key(call)).unwrap_or(&0))
        .collect()
}

#[test]
fn indexed_counts_preserve_aliases_gates_and_progress_boundaries() {
    let mut run = run();
    run.invocations = (0..348).map(read).collect();
    // A project-instruction gate does not count as an executed repeated call.
    for invocation in run.invocations.iter_mut().step_by(67) {
        invocation.result = Some(ToolResult::error(
            Outcome::CancelledBeforeStart,
            "project_instructions_required",
        ));
    }
    let calls: Vec<_> = (0..128).map(|n| read(n).call).collect();
    assert_eq!(old_counts(&run, &calls), indexed_counts(&run, &calls));

    let mut mutation = read(348);
    mutation.call.name = "files.edit".into();
    mutation.definition.as_mut().unwrap().effect = Effect::Write;
    mutation.result = Some(ToolResult::ok(json!({"workspace_changed":true})));
    run.invocations.push(mutation);
    assert_eq!(indexed_counts(&run, &calls), vec![0; 128]);
    assert_eq!(old_counts(&run, &calls), indexed_counts(&run, &calls));

    for _ in 0..3 {
        run.invocations.push(read(0));
    }
    assert_eq!(harness::repetitions(&run, &calls[0]), 3);
    let mut changed = read(0);
    changed.result.as_mut().unwrap().data["sha256"] = json!("version-b");
    run.invocations.push(changed.clone());
    run.invocations.push(changed);
    assert_eq!(harness::repetitions(&run, &calls[0]), 2);
    assert_eq!(old_counts(&run, &calls), indexed_counts(&run, &calls));

    let mut denied = read(0);
    denied.result = Some(ToolResult::error(Outcome::Denied, "path_denied"));
    run.invocations.push(denied);
    assert_eq!(old_counts(&run, &calls), indexed_counts(&run, &calls));

    let mut polling = run.clone();
    polling.invocations.clear();
    for free in [0, 1, 0] {
        let mut status = read(0);
        status.call.name = "agents.status".into();
        status.call.arguments = json!({});
        status.result = Some(ToolResult::ok(json!({"agents":[],
            "execution":{"free_model_slots":free}})));
        polling.invocations.push(status);
    }
    let call = &polling.invocations[0].call;
    assert_eq!(harness::repetitions(&polling, call), 3);
    assert_eq!(
        harness::repetition_counts(&polling)[&working_state::call_key(call)],
        3
    );

    // Keep the live-job exception out of the generic index: callers must use
    // repetitions() for status tools, as completion::observation does.
    for name in ["process.status", "agents.status"] {
        let mut status = read(0);
        status.call.name = name.into();
        status.call.arguments = json!({"id":"owned-job"});
        status.result = Some(ToolResult::ok(json!({"status":"running"})));
        run.invocations
            .extend([status.clone(), status.clone(), status.clone()]);
        assert_eq!(harness::repetitions(&run, &status.call), 1);
        assert_eq!(
            harness::repetition_counts(&run)[&working_state::call_key(&status.call)],
            3
        );
    }
}

#[test]
fn count_preparation_comparison_has_identical_results() {
    let mut run = run();
    run.invocations = (0..348).map(read).collect();
    let calls: Vec<_> = (0..128).map(|n| read(n).call).collect();
    let mut old = Vec::new();
    let start = Instant::now();
    for _ in 0..3 {
        old = old_counts(&run, &calls);
    }
    let old_ms = start.elapsed().as_secs_f64() * 1000.0 / 3.0;
    let mut indexed = Vec::new();
    let start = Instant::now();
    for _ in 0..3 {
        indexed = indexed_counts(&run, &calls);
    }
    let indexed_ms = start.elapsed().as_secs_f64() * 1000.0 / 3.0;
    assert_eq!(old, indexed);
    eprintln!(
        "repetition counting only: 348 invocations, 128 distinct queries, 3 iterations; old_mean_ms={old_ms:.3}, indexed_mean_ms={indexed_ms:.3}; debug_assertions={}",
        cfg!(debug_assertions)
    );
}

#[test]
#[ignore = "set WW_REPLAY_SNAPSHOT; only compares repetition counts, no model requests"]
fn saved_snapshot_count_equivalence() {
    let session: SessionData = serde_json::from_slice(
        &std::fs::read(std::env::var("WW_REPLAY_SNAPSHOT").expect("WW_REPLAY_SNAPSHOT")).unwrap(),
    )
    .unwrap();
    let run = session.runs.last().unwrap();
    let calls: Vec<_> = run
        .invocations
        .iter()
        .map(|invocation| &invocation.call)
        .filter(|call| !matches!(call.name.as_str(), "process.status" | "agents.status"))
        .map(|call| (working_state::call_key(call), call.clone()))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect();
    assert_eq!(old_counts(run, &calls), indexed_counts(run, &calls));
}
