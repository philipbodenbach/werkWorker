use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use werk_worker::{
    context::{ContextBuilder, DefaultContextBuilder, estimate_request, validate_messages},
    harness::{CompletionCheck, HarnessConfig, HarnessMode},
    policy::*,
    profiles::AgentProfile,
    providers::*,
    sessions::*,
    tools::*,
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
fn profile(names: &[&str]) -> AgentProfile {
    AgentProfile {
        schema_version: 1,
        name: "harness-fixture".into(),
        prompt: "Preserve project constraint ALPHA.".into(),
        tools: names.iter().map(|s| s.to_string()).collect(),
        skills: vec![],
    }
}
fn policy() -> HostPolicy {
    HostPolicy {
        workspace_writable: true,
        grants: ["files.read", "files.write", "files.edit"]
            .into_iter()
            .map(|tool| CapabilityGrant {
                tool: tool.into(),
                version: "1".into(),
                resource: ".".into(),
                arguments_hash: None,
                expires_at_ms: None,
                preapproved: true,
            })
            .collect(),
        ..Default::default()
    }
}

struct Probe {
    calls: Arc<AtomicUsize>,
    pass_after: usize,
}
#[async_trait]
impl Tool for Probe {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "fixture.check".into(),
            version: "1".into(),
            description: "Host fixture check".into(),
            input_schema: json!({"type":"object"}),
            effect: Effect::Internal,
            timeout_ms: 1000,
            output_limit: 1024,
            concurrency_safe: true,
        }
    }
    async fn execute(&self, _: Value, _: &ToolContext) -> Result<ToolResult> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(ToolResult::ok(json!({"passed":n >= self.pass_after})))
    }
}
fn registry(calls: Arc<AtomicUsize>, pass_after: usize) -> ToolRegistry {
    let mut tools = ToolRegistry::builtins().unwrap();
    tools
        .register(Arc::new(Probe { calls, pass_after }))
        .unwrap();
    tools
}
fn checked() -> HarnessConfig {
    HarnessConfig {
        completion: Some(CompletionCheck {
            tool: "fixture.check".into(),
            arguments: json!({}),
            result_schema: json!({"type":"object","properties":{"passed":{"const":true}},"required":["passed"]}),
            max_corrections: 1,
        }),
        ..HarnessConfig::minimal()
    }
}

#[tokio::test]
async fn adaptive_readonly_and_coding_keep_stable_authority_and_automatic_continuation() {
    for coding in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note"), "untrusted document evidence").unwrap();
        let action = if coding {
            call(
                "files.write",
                json!({"path":"result", "content":"complete implementation"}),
            )
        } else {
            call("files.read", json!({"path":"note"}))
        };
        let provider = Arc::new(ScriptedProvider::new(vec![
            action,
            ScriptedTurn::text("Done with evidence"),
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .harness(HarnessConfig::minimal())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                profile(if coding {
                    &["files.write", "files.edit"]
                } else {
                    &["files.read"]
                }),
                dir.path(),
                policy(),
                json!({"owner":"kept"}),
            )
            .await
            .unwrap();
        let run = session
            .submit(
                "Do the task; preserve BETA",
                "start",
                Limits::default(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].messages[0], requests[1].messages[0]);
        let system = &requests[0].messages[0].content;
        assert!(system.contains("ALPHA"));
        for absent in [
            "process.run",
            "skeleton",
            "F5",
            "/permissions",
            "Remaining:",
        ] {
            assert!(!system.contains(absent), "{absent}");
        }
        assert!(requests[1].messages.iter().any(|m| m.role == Role::Tool));
        assert_eq!(
            requests[1]
                .messages
                .iter()
                .filter(|m| m.role == Role::User)
                .count(),
            1
        );
        for request in &requests {
            validate_messages(&request.messages).unwrap();
        }
        if coding {
            assert_eq!(
                std::fs::read_to_string(dir.path().join("result")).unwrap(),
                "complete implementation"
            );
        }
        assert_eq!(session.snapshot().await.unwrap().metadata["owner"], "kept");
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn output_limit_feedback_is_transient_and_does_not_execute_discarded_calls() {
    let dir = tempfile::tempdir().unwrap();
    let mut truncated = call("files.write", json!({"path":"discarded","content":"never"}));
    truncated.deltas[2] = ModelDelta::Finish {
        reason: "length".into(),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        call("files.write", json!({"path":"saved","content":"one"})),
        truncated,
        call(
            "files.edit",
            json!({"path":"saved","expected_hash":hash("one"),"old":"one","new":"two"}),
        ),
        ScriptedTurn::text("Done"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .harness(HarnessConfig::minimal())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(&["files.write", "files.edit"]),
            dir.path(),
            policy(),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit("Implement", "start", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert!(!dir.path().join("discarded").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("saved")).unwrap(),
        "two"
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    let feedback = |request: &ModelRequest| {
        request
            .messages
            .iter()
            .any(|m| m.content.contains("\"event\":\"generation_output_limit\""))
    };
    assert!(!feedback(&requests[1]));
    assert!(feedback(&requests[2]));
    assert!(!feedback(&requests[3]));
    assert!(
        requests
            .iter()
            .all(|r| r.messages[0] == requests[0].messages[0])
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn configured_completion_is_bounded_persisted_and_respects_policy_and_budgets() {
    for case in [
        "accept",
        "repair",
        "fail",
        "deny",
        "tool_budget",
        "step_budget",
        "request_budget",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let pass_after = if case == "accept" {
            1
        } else if case == "repair" {
            2
        } else {
            99
        };
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedTurn::text("Candidate"),
            ScriptedTurn::text("Corrected"),
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .harness(checked())
            .tools(registry(calls.clone(), pass_after))
            .build()
            .await
            .unwrap();
        let mut policy = HostPolicy::default();
        if case == "deny" {
            policy.hard_denied_tools.push("fixture.check".into());
        }
        let session = engine
            .create_session(profile(&["fixture.check"]), dir.path(), policy, Value::Null)
            .await
            .unwrap();
        let mut limits = Limits::default();
        if case == "tool_budget" {
            limits.max_tool_invocations = 0;
        }
        if case == "step_budget" {
            limits.max_steps = 1;
        }
        if case == "request_budget" {
            limits.max_model_requests = 1;
        }
        let run = session.submit("Task", "start", limits, None).await.unwrap();
        let result = run.wait().await.unwrap();
        if ["accept", "repair"].contains(&case) {
            assert_eq!(
                result.status,
                RunStatus::Completed,
                "{case}: {:?}",
                result.error
            );
        } else {
            assert_eq!(
                result.error.unwrap().code,
                match case {
                    "fail" => "completion_failed",
                    "deny" => "completion_blocked",
                    _ => case,
                },
                "{case}"
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            match case {
                "deny" | "tool_budget" | "step_budget" => 0,
                "accept" | "request_budget" => 1,
                _ => 2,
            }
        );
        let data = session.snapshot().await.unwrap();
        validate_messages(&data.messages).unwrap();
        if case == "repair" {
            assert_eq!(
                data.metadata["_werkworker_harness"][&run.id]["checks"][0]["verdict"],
                "findings"
            );
            assert_eq!(
                data.metadata["_werkworker_harness"][&run.id]["checks"][1]["verdict"],
                "accepted"
            );
            assert_eq!(
                provider.requests()[1]
                    .messages
                    .iter()
                    .filter(|m| m.role == Role::User)
                    .count(),
                1
            );
        }
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn final_candidate_resume_checks_once_and_unknown_outcomes_never_replay() {
    for phase in ["candidate", "prepared", "finished", "running", "accepted"] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text("Candidate")]));
        let engine = WorkerEngine::builder(provider)
            .harness(checked())
            .tools(registry(calls.clone(), 1))
            .store(store.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                profile(&["fixture.check"]),
                dir.path(),
                HostPolicy::default(),
                Value::Null,
            )
            .await
            .unwrap();
        let run = session
            .submit("Task", "start", Limits::default(), None)
            .await
            .unwrap();
        assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
        let mut snapshot = session.snapshot().await.unwrap();
        engine.close().await.unwrap();
        // Reconstruct exact durable crash boundaries, not a substitute executor.
        let revision = snapshot.revision;
        let r = &mut snapshot.runs[0];
        r.status = RunStatus::Running;
        let state = &mut snapshot.metadata["_werkworker_harness"][&run.id];
        if phase == "candidate" {
            r.invocations.clear();
            r.metrics.tool_invocations = 0;
            r.metrics.steps = 1;
            snapshot.messages.truncate(2);
            state["checks"] = json!([]);
            state["messages"] = json!([]);
        } else {
            if phase != "accepted" {
                state["checks"][0]["verdict"] = Value::Null;
            }
            if matches!(phase, "prepared" | "running") {
                r.invocations[0].result = None;
                r.invocations[0].status = if phase == "running" {
                    InvocationStatus::Running
                } else {
                    InvocationStatus::Prepared
                };
                snapshot.messages.truncate(3);
            }
        }
        snapshot.revision += 1;
        store.save(&snapshot, Some(revision)).await.unwrap();
        calls.store(0, Ordering::SeqCst);
        let provider = Arc::new(ScriptedProvider::default());
        // Different engine defaults must not change the interrupted run.
        let engine = WorkerEngine::builder(provider.clone())
            .tools(registry(calls.clone(), 1))
            .store(store)
            .build()
            .await
            .unwrap();
        let resumed = engine.resume_run(&snapshot.id, &run.id).await;
        if phase == "running" {
            assert_eq!(resumed.err().unwrap().code, "outcome_unknown");
        } else {
            assert_eq!(
                resumed.unwrap().wait().await.unwrap().status,
                RunStatus::Completed,
                "{phase}"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                usize::from(matches!(phase, "candidate" | "prepared"))
            );
        }
        assert!(provider.requests().is_empty());
        let data = engine
            .open_session(&snapshot.id)
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap();
        assert_eq!(
            harness::config(&data, &data.runs[0]).unwrap().mode,
            HarnessMode::Adaptive
        );
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn old_runs_resume_as_classic_under_adaptive_engine_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(InMemoryStore::default());
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
        "Done",
    )])))
    .store(store.clone())
    .build()
    .await
    .unwrap();
    let session = engine
        .create_session(profile(&[]), dir.path(), HostPolicy::default(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("Task", "start", Limits::default(), None)
        .await
        .unwrap();
    run.wait().await.unwrap();
    let mut data = session.snapshot().await.unwrap();
    engine.close().await.unwrap();
    data.metadata = Value::Null; // Genuine pre-harness snapshot.
    let revision = data.revision;
    data.runs[0].status = RunStatus::Running;
    data.revision += 1;
    store.save(&data, Some(revision)).await.unwrap();
    let provider = Arc::new(ScriptedProvider::default());
    let engine = WorkerEngine::builder(provider.clone())
        .harness(checked())
        .tools(registry(Arc::new(AtomicUsize::new(0)), 1))
        .store(store)
        .build()
        .await
        .unwrap();
    assert_eq!(
        engine
            .resume_run(&data.id, &run.id)
            .await
            .unwrap()
            .wait()
            .await
            .unwrap()
            .status,
        RunStatus::Completed
    );
    assert!(provider.requests().is_empty());
    assert_eq!(
        harness::config(&data, &data.runs[0]).unwrap().mode,
        HarnessMode::Classic
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn compaction_restores_full_skill_content_and_never_promotes_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        call("skills.load", json!({"name":"document-analysis"})),
        ScriptedTurn::text("Done"),
    ]));
    let engine = WorkerEngine::builder(provider)
        .harness(HarnessConfig::minimal())
        .build()
        .await
        .unwrap();
    let mut agent = profile(&["skills.load"]);
    agent.skills.push("document-analysis".into());
    let session = engine
        .create_session(agent, dir.path(), HostPolicy::default(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("Preserve BETA", "start", Limits::default(), None)
        .await
        .unwrap();
    run.wait().await.unwrap();
    let mut data = session.snapshot().await.unwrap();
    let loaded = data.runs[0].invocations[0].message_index;
    data.compactions.push(CompactionRecord {
        id: id("compact"),
        run_id: run.id.clone(),
        attempt_id: id("attempt"),
        message_indices: vec![loaded, loaded + 1],
        source_hash: "fixture".into(),
        summary: "UNTRUSTED SUMMARY".into(),
    });
    data.messages.push(Message::text(
        Role::Developer,
        "Retain developer constraint",
    ));
    let (request, _) = DefaultContextBuilder
        .build(&data, &data.runs[0], vec![])
        .await
        .unwrap();
    validate_messages(&request.messages).unwrap();
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.role == Role::Developer && m.content == "Retain developer constraint")
    );
    assert!(!request.messages[0].content.contains("UNTRUSTED SUMMARY"));
    let restored = request
        .messages
        .iter()
        .find(|m| m.content.contains("retained_skill"))
        .unwrap();
    assert_eq!(restored.role, Role::Assistant);
    let value: Value = serde_json::from_str(&restored.content).unwrap();
    assert_eq!(
        value["data"]["skill"]["body"],
        data.skill_snapshots["document-analysis"].body
    );
    engine.close().await.unwrap();
}

/// Reproducible fixture comparison; counts ALL wire messages and tool schemas.
#[tokio::test]
async fn legacy_configurations_migrate_to_identical_adaptive_contexts() {
    for name in AgentProfile::names() {
        let mut measurements = Vec::new();
        for mode in [HarnessMode::Classic, HarnessMode::Minimal] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("note"), "alpha: 2\nbeta: 3\n").unwrap();
            std::fs::write(dir.path().join("math.py"), "def add(a, b): return a - b\n").unwrap();
            let (input, turns) = match name {
                "general-readonly" => (
                    "Compare the two counts in note with a source citation",
                    vec![
                        call("files.read", json!({"path":"note"})),
                        ScriptedTurn::text("beta exceeds alpha by 1 (note:1-2)."),
                    ],
                ),
                "coding-basic" => (
                    "Correct add in math.py and inspect the saved change",
                    vec![
                        call("files.read", json!({"path":"math.py"})),
                        call(
                            "files.edit",
                            json!({"path":"math.py","old":"a - b","new":"a + b","expected_hash":hash("def add(a, b): return a - b\n")}),
                        ),
                        call("files.read", json!({"path":"math.py"})),
                        ScriptedTurn::text(
                            "Changed subtraction to addition; inspected the saved file. No executable test was run.",
                        ),
                    ],
                ),
                _ => (
                    "Write a ticket preview from note, without submitting it",
                    vec![
                        call("files.read", json!({"path":"note"})),
                        call(
                            "files.write",
                            json!({"path":"preview.txt","content":"Review beta count (3), source note:2. Acceptance: confirm count."}),
                        ),
                        ScriptedTurn::text("Preview saved; no external submission."),
                    ],
                ),
            };
            let provider = Arc::new(ScriptedProvider::new(turns));
            let engine = WorkerEngine::builder(provider.clone())
                .harness(HarnessConfig {
                    mode,
                    ..Default::default()
                })
                .build()
                .await
                .unwrap();
            let session = engine
                .create_session(
                    AgentProfile::bundled(name).unwrap(),
                    dir.path(),
                    HostPolicy {
                        workspace_writable: true,
                        tool_permissions: AgentProfile::bundled(name)
                            .unwrap()
                            .tools
                            .into_iter()
                            .map(|tool| (tool, ToolPermission::Allow))
                            .collect(),
                        ..Default::default()
                    },
                    Value::Null,
                )
                .await
                .unwrap();
            let run = session
                .submit(input, "start", Limits::default(), None)
                .await
                .unwrap();
            let result = run.wait().await.unwrap();
            assert_eq!(result.status, RunStatus::Completed);
            let requests = provider.requests();
            let tools_bytes = serde_json::to_vec(&requests[0].tools.iter().map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.input_schema}})).collect::<Vec<_>>()).unwrap().len();
            let state = run.status().await.unwrap();
            assert!(state.invocations.iter().all(|i| {
                i.result
                    .as_ref()
                    .is_some_and(|r| r.status == Outcome::Succeeded)
            }));
            if name == "coding-basic" {
                assert_eq!(
                    std::fs::read_to_string(dir.path().join("math.py")).unwrap(),
                    "def add(a, b): return a + b\n"
                );
            }
            if name == "automation-basic" {
                assert!(dir.path().join("preview.txt").exists());
            }
            let record = json!({"profile":name,"legacy_config":mode,"effective_mode":"adaptive","source":"wire_bytes_estimate_fixture", "system_bytes":requests[0].messages[0].content.len(), "tools":requests[0].tools.len(),"tool_schema_bytes":tools_bytes,"request_estimates":requests.iter().map(estimate_request).collect::<Vec<_>>(),"model_requests":result.metrics.model_requests,"toolcalls":result.metrics.tool_invocations,"retries":state.attempts.iter().filter(|a| a.error.is_some()).count(),"compactions":state.contexts.iter().filter(|c| !c.omitted_messages.is_empty()).count(),"status":result.status,"wall_ms":result.metrics.total_ms});
            println!("HARNESS_MEASUREMENT {record}");
            measurements.push(requests.iter().map(estimate_request).sum::<usize>());
            engine.close().await.unwrap();
        }
        assert_eq!(measurements[1], measurements[0]);
    }
}

#[tokio::test]
async fn new_read_observations_allow_research_but_unchanged_reads_and_denials_are_bounded() {
    for case in ["research", "unchanged", "denied"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "a").unwrap();
        std::fs::write(dir.path().join("b"), "b").unwrap();
        let turns = if case == "research" {
            vec![
                call("files.read", json!({"path":"a"})),
                call("files.read", json!({"path":"b"})),
                call("files.read", json!({"path":"a"})),
            ]
        } else {
            vec![
                call("files.read", json!({"path":"a"})),
                call("files.read", json!({"path":"a"})),
            ]
        };
        let provider = Arc::new(ScriptedProvider::new(
            turns
                .into_iter()
                .chain([ScriptedTurn::text("Done")])
                .collect(),
        ));
        let engine = WorkerEngine::builder(provider)
            .harness(HarnessConfig::minimal())
            .build()
            .await
            .unwrap();
        let mut policy = policy();
        if case == "denied" {
            policy.hard_denied_tools.push("files.read".into());
        }
        let session = engine
            .create_session(profile(&["files.read"]), dir.path(), policy, Value::Null)
            .await
            .unwrap();
        let run = session
            .submit(
                "Research",
                "start",
                Limits {
                    max_repeated_calls: 1,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        if case == "research" {
            assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
        } else {
            assert_eq!(result.error.unwrap().code, "repetition_limit");
        }
        engine.close().await.unwrap();
    }
}

#[test]
fn documented_runtime_measurements_survive_json_and_sse_without_backend_text() {
    let value = json!({"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"delta":{"content":"ok"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":25,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":17}},
        "werk":{"timings":{"total_seconds":1.5,"load_seconds":-1,"prompt_seconds":"unknown","cached_prompt_tokens":17},"backend_diagnostics":{"secret":"never persist"}}});
    for stream in [false, true] {
        let deltas = if stream {
            SseDecoder::new(10000)
                .push(format!("data: {value}\n\ndata: [DONE]\n\n").as_bytes())
                .unwrap()
        } else {
            decode_completion(&value).unwrap()
        };
        let mut assembler = ResponseAssembler::default();
        for delta in deltas {
            assembler.push(&delta, 10000).unwrap();
        }
        let (_, usage, metadata) = assembler.finish().unwrap();
        assert_eq!(usage.unwrap().cached_tokens, Some(17));
        assert_eq!(
            metadata,
            json!({"werk":{"timings":{"total_seconds":1.5,"cached_prompt_tokens":17}}})
        );
    }
}

#[tokio::test]
async fn completion_requires_real_approval_and_cannot_be_an_external_write_probe() {
    use futures_util::StreamExt;
    let mut config = checked();
    config.completion.as_mut().unwrap().tool = "files.write".into();
    let invalid = WorkerEngine::builder(Arc::new(ScriptedProvider::default()))
        .harness(config)
        .build()
        .await;
    assert_eq!(invalid.err().unwrap().code, "invalid_harness");
    for allow in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("result"), "ready").unwrap();
        let mut config = checked();
        config.completion = Some(CompletionCheck {
            tool: "files.read".into(),
            arguments: json!({"path":"result"}),
            result_schema: json!({"properties":{"content":{"const":"ready"}},"required":["content"]}),
            max_corrections: 1,
        });
        let provider = Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text("Candidate")]));
        let engine = WorkerEngine::builder(provider.clone())
            .harness(config)
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                profile(&["files.read"]),
                dir.path(),
                HostPolicy {
                    request_approvals: true,
                    ..Default::default()
                },
                Value::Null,
            )
            .await
            .unwrap();
        let run = session
            .submit("Check result", "start", Limits::default(), None)
            .await
            .unwrap();
        let mut events = run.events(0);
        let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let EventKind::ApprovalRequested { request } =
                    events.next().await.unwrap().unwrap().event
                {
                    break request;
                }
            }
        })
        .await
        .unwrap();
        run.respond_to_approval(
            &request.request_id,
            if allow {
                ApprovalDecision::AllowOnce
            } else {
                ApprovalDecision::Deny
            },
            "decision",
        )
        .await
        .unwrap();
        let result = run.wait().await.unwrap();
        if allow {
            assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
        } else {
            assert_eq!(result.error.unwrap().code, "completion_blocked");
        }
        assert_eq!(provider.requests().len(), 1);
        engine.close().await.unwrap();
    }
}

struct PollingProvider;
#[async_trait]
impl ModelProvider for PollingProvider {
    fn identity(&self) -> String {
        "polling-fixture".into()
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let result = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .map(|m| serde_json::from_str::<ToolResult>(&m.content).unwrap());
        let mut turn = match result {
            None => call("process.start", json!({"argv":["/bin/sleep","0.2"]})),
            Some(result) if result.data["status"] == "finished" => {
                ScriptedTurn::text("Job finished")
            }
            Some(result) => call("process.status", json!({"job_id":result.data["job_id"]})),
        };
        turn.delay_ms = 5;
        Ok(Box::pin(async_stream::stream! {
            for delta in turn.deltas {
                tokio::time::sleep(std::time::Duration::from_millis(turn.delay_ms)).await;
                yield Ok(delta);
            }
        }))
    }
}

#[tokio::test]
async fn active_owned_job_polling_does_not_require_workspace_writes() {
    if let Err(error) = runtime::Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        eprintln!("SKIP real sandbox: {error}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let engine = WorkerEngine::builder(Arc::new(PollingProvider))
        .harness(HarnessConfig::minimal())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(&["process.start", "process.status"]),
            dir.path(),
            HostPolicy {
                grants: vec![CapabilityGrant {
                    tool: "process.start".into(),
                    version: "1".into(),
                    resource: "workspace".into(),
                    arguments_hash: None,
                    expires_at_ms: None,
                    preapproved: true,
                }],
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Wait for the owned job",
            "start",
            Limits {
                max_repeated_calls: 1,
                max_wall_ms: 5000,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(result.text.as_deref(), Some("Job finished"));
    assert!(
        run.status()
            .await
            .unwrap()
            .invocations
            .iter()
            .filter(|i| i.call.name == "process.status")
            .count()
            > 2
    );
    engine.close().await.unwrap();
}

fn hints(request: &ModelRequest, kind: &str) -> usize {
    request
        .messages
        .iter()
        .filter(|m| {
            serde_json::from_str::<Value>(&m.content)
                .is_ok_and(|v| v["source"] == "werk_worker.harness" && v["event"] == kind)
        })
        .count()
}

#[tokio::test]
async fn truncated_tool_then_limited_generation_have_independent_receipts() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("large"),
        "original evidence line\n".repeat(3000),
    )
    .unwrap();
    let mut limited = call("files.write", json!({"path":"discarded","content":"never"}));
    limited.deltas[2] = ModelDelta::Finish {
        reason: "length".into(),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        call("files.write", json!({"path":"saved","content":"confirmed"})),
        call("files.read", json!({"path":"large"})),
        limited,
        call(
            "files.read",
            json!({"path":"large","start_line":2000,"end_line":2001}),
        ),
        ScriptedTurn::text("Done"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(&["files.read", "files.write"]),
            dir.path(),
            policy(),
            Value::Null,
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Inspect while preserving saved work",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let data = session.snapshot().await.unwrap();
    let state = &data.metadata["_werkworker_harness"][&run.id]["adaptive"]["aids"];
    let requests = provider.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(hints(&requests[2], "tool_result_truncated"), 1);
    assert_eq!(hints(&requests[3], "tool_result_truncated"), 0);
    assert_eq!(hints(&requests[3], "generation_output_limit"), 1);
    assert_eq!(hints(&requests[4], "generation_output_limit"), 0);
    let r = &data.runs[0];
    assert!(r.invocations[1].result.as_ref().unwrap().truncated);
    assert_eq!(
        state["tool_result_truncated"]["trigger"],
        r.invocations[1].invocation_id
    );
    assert_eq!(
        state["generation_output_limit"]["trigger"],
        r.attempts[2].attempt_id
    );
    assert_ne!(
        state["tool_result_truncated"]["delivered_step"],
        state["generation_output_limit"]["delivered_step"]
    );
    assert_eq!(state["tool_result_truncated"]["episodes"], 1);
    assert_eq!(state["generation_output_limit"]["episodes"], 1);
    assert_eq!(r.metrics.tool_invocations, 3);
    assert!(!dir.path().join("discarded").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("saved")).unwrap(),
        "confirmed"
    );
    for request in &requests {
        validate_messages(&request.messages).unwrap();
    }
    let estimates: Vec<_> = requests.iter().map(estimate_request).collect();
    let serialized_bytes: Vec<_> = requests
        .iter()
        .map(|request| serde_json::to_vec(request).unwrap().len())
        .collect();
    let tool_schema_bytes: Vec<_> = requests
        .iter()
        .map(|request| serde_json::to_vec(&request.tools).unwrap().len())
        .collect();
    assert_eq!(
        r.contexts
            .iter()
            .map(|snapshot| snapshot.estimated_tokens)
            .collect::<Vec<_>>(),
        estimates
    );
    println!(
        "FOLLOWUP_RECOVERY_MEASURE {}",
        json!({"request_estimates":estimates,"total_estimate":estimates.iter().sum::<usize>(),"serialized_request_bytes":serialized_bytes,"total_serialized_request_bytes":serialized_bytes.iter().sum::<usize>(),"tool_schema_bytes":tool_schema_bytes,"model_requests":r.metrics.model_requests,"tool_proposals":r.metrics.tool_invocations,"executed_tools":r.invocations.iter().filter(|i| i.result.as_ref().is_some_and(|result| result.status == Outcome::Succeeded)).count(),"status":"completed","verification":"prior write preserved; discarded call absent; both hints reference actual receipts; context snapshots match complete request estimates"})
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn legacy_output_aids_migrate_across_crash_without_redelivery_or_budget_reset() {
    for (generation, consumed, observed, conflicting) in [
        (false, false, false, false),
        (false, true, false, false),
        (false, true, true, false),
        (true, false, false, false),
        (true, true, false, false),
        (true, false, false, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("large"), "line of evidence\n".repeat(3000)).unwrap();
        let store = Arc::new(InMemoryStore::default());
        let limited = ScriptedTurn {
            deltas: vec![
                ModelDelta::Finish {
                    reason: "length".into(),
                },
                ModelDelta::Done,
            ],
            delay_ms: 0,
            error: None,
        };
        let mut slow = ScriptedTurn::text("in flight");
        slow.delay_ms = 60_000;
        let provider = Arc::new(ScriptedProvider::new(vec![
            if generation {
                limited
            } else {
                call("files.read", json!({"path":"large"}))
            },
            slow,
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .store(store.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(&["files.read"]), dir.path(), policy(), Value::Null)
            .await
            .unwrap();
        let run = session
            .submit("Inspect", "start", Limits::default(), None)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while provider.requests().len() < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut saved = session.snapshot().await.unwrap();
        engine.close().await.unwrap();
        let kind = if generation {
            "generation_output_limit"
        } else {
            "tool_result_truncated"
        };
        let state = &mut saved.metadata["_werkworker_harness"][&run.id];
        state["version"] = json!(2);
        let mut old = state["adaptive"]["aids"]
            .as_object_mut()
            .unwrap()
            .remove(kind)
            .unwrap();
        if generation {
            old["scope"] = json!(if conflicting {
                "files.read:truncated"
            } else if consumed {
                "unknown_scope"
            } else {
                "generation_limit"
            });
        } else if consumed {
            old["active"] = json!(false);
            old["rearm_at"] = json!(999);
        } else {
            old["scope"] = json!("unknown_scope");
        }
        state["adaptive"]["aids"]["output_limit"] = old.clone();
        let in_flight = saved.runs[0].attempts.last_mut().unwrap();
        if consumed {
            in_flight.error = Some(WorkerError::new(
                "model_output_limit",
                "discarded at crash boundary",
            ));
        } else {
            saved.runs[0].attempts.pop();
        }
        if observed {
            // The legacy refresh already consumed the failed attempt and
            // suppressed it against the tool hint before the crash.
            saved.metadata["_werkworker_harness"][&run.id]["adaptive"]["attempts"] =
                json!(saved.runs[0].attempts.len());
        }
        let previous_requests = saved.runs[0].metrics.model_requests;
        let revision = store.load(&saved.id).await.unwrap().revision;
        saved.revision = revision + 1;
        store.save(&saved, Some(revision)).await.unwrap();
        let next = Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text("Done")]));
        let reopened = WorkerEngine::builder(next.clone())
            .store(store.clone())
            .build()
            .await
            .unwrap();
        let result = reopened
            .resume_run(&saved.id, &run.id)
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
        let request = &next.requests()[0];
        assert_eq!(hints(request, kind), usize::from(!consumed && !conflicting));
        if conflicting {
            let notice = request
                .messages
                .iter()
                .filter_map(|message| serde_json::from_str::<Value>(&message.content).ok())
                .find(|message| message["event"] == "output_limit")
                .unwrap();
            assert!(
                notice["data"]["detail"]
                    .as_str()
                    .unwrap()
                    .contains("ambiguous provenance")
            );
            assert_eq!(hints(request, "tool_result_truncated"), 0);
        }
        if !generation && consumed {
            assert_eq!(hints(request, "generation_output_limit"), 1);
        }
        let data = reopened
            .open_session(&saved.id)
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap();
        let migrated = &data.metadata["_werkworker_harness"][&run.id]["adaptive"]["aids"][kind];
        assert_eq!(migrated["episodes"], old["episodes"]);
        assert_eq!(migrated["trigger"], old["trigger"]);
        assert_eq!(data.runs[0].metrics.model_requests, previous_requests + 1);
        assert_eq!(
            data.runs[0].limits.max_retries,
            saved.runs[0].limits.max_retries
        );
        if observed {
            let attempt = &saved.runs[0].attempts.last().unwrap().attempt_id;
            let code =
                format!("harness_generation_output_limit_restored_legacy_suppression:{attempt}");
            assert!(data.events.iter().any(|event| {
                matches!(&event.event, EventKind::Diagnostic { code: actual } if actual == &code)
            }));
        }
        assert_eq!(
            serde_json::to_value(&data.events[..saved.events.len()]).unwrap(),
            serde_json::to_value(&saved.events).unwrap()
        );
        reopened.close().await.unwrap();
    }
}

#[tokio::test]
async fn adaptive_validation_is_scoped_then_removed_without_oscillation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note"), "evidence").unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        call("files.read", json!({})),
        call("files.read", json!({})),
        call("files.read", json!({"path":"note"})),
        call("files.read", json!({})),
        ScriptedTurn::text("Done"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            profile(&["files.read"]),
            dir.path(),
            policy(),
            json!("old scalar metadata"),
        )
        .await
        .unwrap();
    let run = session
        .submit("Read note", "start", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let requests = provider.requests();
    assert_eq!(
        requests
            .iter()
            .map(|r| hints(r, "tool_validation"))
            .collect::<Vec<_>>(),
        [0, 1, 0, 0, 0]
    );
    assert!(
        requests
            .iter()
            .all(|r| r.messages[0] == requests[0].messages[0])
    );
    let data = session.snapshot().await.unwrap();
    assert_eq!(
        data.metadata["_werkworker_legacy_metadata"],
        "old scalar metadata"
    );
    let state = &data.metadata["_werkworker_harness"][&run.id];
    assert_eq!(state["version"], 3);
    assert_eq!(state["config"]["mode"], "adaptive");
    assert_eq!(
        state["adaptive"]["aids"]["tool_validation"]["active"],
        false
    );
    assert_eq!(state["adaptive"]["aids"]["tool_validation"]["episodes"], 1);
    assert!(data.events.iter().any(|e| matches!(&e.event, EventKind::Diagnostic{code} if code.starts_with("harness_tool_validation_activated:invocation"))));
    assert!(
        data.events.iter().any(
            |e| matches!(&e.event, EventKind::Diagnostic{code} if code.contains("deactivated"))
        )
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn adaptive_missing_information_and_repetition_use_observed_receipts() {
    for missing in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "A").unwrap();
        std::fs::write(dir.path().join("b"), "B").unwrap();
        let turns = if missing {
            vec![
                call("files.read", json!({"path":"absent"})),
                call("files.read", json!({"path":"a"})),
                ScriptedTurn::text("Done"),
            ]
        } else {
            vec![
                call("files.read", json!({"path":"a"})),
                call("files.read", json!({"path":"a"})),
                call("files.read", json!({"path":"b"})),
                ScriptedTurn::text("Done"),
            ]
        };
        let provider = Arc::new(ScriptedProvider::new(turns));
        let engine = WorkerEngine::builder(provider.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(&["files.read"]), dir.path(), policy(), Value::Null)
            .await
            .unwrap();
        let run = session
            .submit("Inspect", "start", Limits::default(), None)
            .await
            .unwrap();
        assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
        let kind = if missing {
            "missing_information"
        } else {
            "repeated_action"
        };
        let counts: Vec<_> = provider.requests().iter().map(|r| hints(r, kind)).collect();
        assert_eq!(
            counts,
            if missing {
                vec![0, 1, 0]
            } else {
                vec![0, 0, 1, 0]
            }
        );
        let data = session.snapshot().await.unwrap();
        assert_eq!(
            data.metadata["_werkworker_harness"][&run.id]["adaptive"]["aids"][kind]["active"],
            false
        );
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn adaptive_runs_without_markdown_or_skills_and_without_precautionary_hints() {
    let dir = tempfile::tempdir().unwrap();
    let project = project::Project::load(dir.path()).await.unwrap();
    assert!(project.instructions.is_empty());
    let mut skills = skills::SkillRegistry::default();
    skills
        .load_directory(&dir.path().join("absent-skill"))
        .await
        .unwrap();
    skills.load_directory(dir.path()).await.unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        call("skills.load", json!({"name":"optional"})),
        ScriptedTurn::text("Done"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .skills(skills)
        .build()
        .await
        .unwrap();
    let mut agent = profile(&["skills.load"]);
    agent.skills = vec!["optional".into()];
    let session = engine
        .create_session(agent, dir.path(), HostPolicy::default(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit(
            "Respond",
            "start",
            Limits {
                max_steps: 2,
                max_tool_invocations: 1,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let data = session.snapshot().await.unwrap();
    assert!(
        data.metadata["_werkworker_harness"][&run.id]["adaptive"]["aids"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    assert!(data.skill_snapshots.is_empty());
    assert_eq!(
        data.runs[0].invocations[0].result.as_ref().unwrap().status,
        Outcome::Succeeded
    );
    for request in provider.requests() {
        assert!(
            !request
                .messages
                .iter()
                .any(|m| m.content.contains("werk_worker.harness"))
        );
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn adaptive_pending_and_delivered_hints_survive_resume() {
    for delivered in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::default());
        let mut slow = ScriptedTurn::text("never completed in original process");
        slow.delay_ms = 60_000;
        let provider = Arc::new(ScriptedProvider::new(vec![
            call("files.read", json!({})),
            slow,
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .store(store.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(&["files.read"]), dir.path(), policy(), Value::Null)
            .await
            .unwrap();
        let run = session
            .submit("Inspect", "start", Limits::default(), None)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while provider.requests().len() < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        // Snapshot the crash boundary, before the in-flight recovery response.
        let mut saved = session.snapshot().await.unwrap();
        assert_eq!(hints(&provider.requests()[1], "tool_validation"), 1);
        engine.close().await.unwrap();
        let revision = store.load(&saved.id).await.unwrap().revision;
        let aid = &mut saved.metadata["_werkworker_harness"][&run.id]["adaptive"]["aids"]["tool_validation"];
        if delivered {
            // A persisted failed response consumed this episode's hint.
            saved.runs[0].attempts.last_mut().unwrap().error =
                Some(WorkerError::new("transport", "interrupted"));
        } else {
            // Context planned, no completed dispatch; reconstruct its pending hint.
            aid["delivered_step"] = Value::Null;
            saved.runs[0].attempts.pop();
        }
        saved.revision = revision + 1;
        store.save(&saved, Some(revision)).await.unwrap();
        let next = Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text("Done")]));
        let reopened = WorkerEngine::builder(next.clone())
            .store(store)
            .build()
            .await
            .unwrap();
        let result = reopened
            .resume_run(&saved.id, &run.id)
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(
            hints(&next.requests()[0], "tool_validation"),
            usize::from(!delivered)
        );
        let data = reopened
            .open_session(&saved.id)
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap();
        assert_eq!(
            data.metadata["_werkworker_harness"][&run.id]["adaptive"]["aids"]["tool_validation"]["episodes"],
            1
        );
        reopened.close().await.unwrap();
    }
}

#[tokio::test]
async fn legacy_snapshots_keep_execution_contract_but_new_followups_are_adaptive() {
    for mode in [None, Some("classic"), Some("minimal"), Some("omitted")] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::default());
        let engine =
            WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
                "seed",
            )])))
            .store(store.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(&[]), dir.path(), HostPolicy::default(), Value::Null)
            .await
            .unwrap();
        let run = session
            .submit("Task", "seed", Limits::default(), None)
            .await
            .unwrap();
        run.wait().await.unwrap();
        let mut saved = session.snapshot().await.unwrap();
        engine.close().await.unwrap();
        let revision = saved.revision;
        saved.runs[0].status = RunStatus::Running;
        saved.runs[0].final_text = None;
        saved.metadata = match mode {
            None => json!({"host":"preserved"}),
            Some(value) => {
                json!({"host":"preserved","_werkworker_harness":{&run.id:{"config":{"mode":value,"local_guidance":"Preserve legacy host guidance"},"messages":[],"checks":[]}}})
            }
        };
        if mode == Some("omitted") {
            saved.metadata["_werkworker_harness"][&run.id]["config"]
                .as_object_mut()
                .unwrap()
                .remove("mode");
        }
        saved.revision += 1;
        store.save(&saved, Some(revision)).await.unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedTurn::text("Resumed"),
            ScriptedTurn::text("Followup"),
        ]));
        let reopened = WorkerEngine::builder(provider.clone())
            .store(store)
            .build()
            .await
            .unwrap();
        assert_eq!(
            reopened
                .resume_run(&saved.id, &run.id)
                .await
                .unwrap()
                .wait()
                .await
                .unwrap()
                .status,
            RunStatus::Completed
        );
        let session = reopened.open_session(&saved.id).await.unwrap();
        let state = session.snapshot().await.unwrap();
        assert_eq!(
            harness::config(&state, &state.runs[0]).unwrap().mode,
            if mode == Some("minimal") {
                HarnessMode::Minimal
            } else {
                HarnessMode::Classic
            }
        );
        assert_eq!(state.metadata["_werkworker_harness"][&run.id]["version"], 3);
        assert_eq!(state.metadata["host"], "preserved");
        assert!(state.events.iter().any(|e| matches!(&e.event,EventKind::Diagnostic{code} if code == "harness_migration_legacy_contract_preserved")));
        let first = &provider.requests()[0].messages[0].content;
        if mode == Some("minimal") {
            assert!(first.contains("Preserve legacy host guidance"));
            assert!(!first.contains("Workspace writes enabled:"));
        } else {
            assert!(first.contains("Workspace writes enabled:"));
        }
        let next = session
            .submit("Next", "next", Limits::default(), None)
            .await
            .unwrap();
        assert_eq!(next.wait().await.unwrap().status, RunStatus::Completed);
        let state = session.snapshot().await.unwrap();
        assert_eq!(
            harness::config(&state, &state.runs[1]).unwrap().mode,
            HarnessMode::Adaptive
        );
        assert!(
            !provider.requests()[1].messages[0]
                .content
                .contains("Build large files across turns")
        );
        reopened.close().await.unwrap();
    }
}

#[tokio::test]
async fn repeated_output_limits_do_not_reinject_the_hint_or_expand_recovery_budget() {
    let dir = tempfile::tempdir().unwrap();
    let limited = || ScriptedTurn {
        deltas: vec![
            ModelDelta::Finish {
                reason: "length".into(),
            },
            ModelDelta::Done,
        ],
        delay_ms: 0,
        error: None,
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        limited(),
        limited(),
        limited(),
        ScriptedTurn::text("must not run"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(profile(&[]), dir.path(), HostPolicy::default(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit(
            "Task",
            "start",
            Limits {
                max_retries: 2,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        run.wait().await.unwrap().error.unwrap().code,
        "model_output_limit"
    );
    assert_eq!(
        provider
            .requests()
            .iter()
            .map(|r| hints(r, "generation_output_limit"))
            .collect::<Vec<_>>(),
        [0, 1, 0]
    );
    assert_eq!(run.status().await.unwrap().metrics.model_requests, 3);
    engine.close().await.unwrap();
}
