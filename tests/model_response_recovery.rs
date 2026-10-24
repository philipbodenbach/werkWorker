use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use werk_worker::{
    context::validate_messages,
    policy::*,
    profiles::AgentProfile,
    providers::*,
    sessions::{InMemoryStore, SessionStore},
    *,
};

enum Reply {
    Turn(ScriptedTurn),
    FeedbackEcho,
    ChangedIdEcho,
    ObservationEcho(&'static str),
    BoundaryEcho,
    PrefillSensitive(ScriptedTurn),
    Empty,
    Pending,
    NeedsBudget(usize, ScriptedTurn),
}

struct RecoveryProvider {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<ModelRequest>>,
    output_ceiling: Option<usize>,
}
impl RecoveryProvider {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
            output_ceiling: None,
        }
    }
    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}
#[async_trait]
impl ModelProvider for RecoveryProvider {
    fn max_output_tokens(&self) -> Option<usize> {
        self.output_ceiling
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tool_calling: Support::Supported,
            streaming: Support::Supported,
            ..Default::default()
        }
    }
    fn identity(&self) -> String {
        "response-recovery-fixture".into()
    }
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        self.requests.lock().unwrap().push(request.clone());
        let reply = self.replies.lock().unwrap().pop_front().unwrap();
        let turn = match reply {
            Reply::Turn(turn) => turn,
            Reply::FeedbackEcho | Reply::ChangedIdEcho => {
                let hint = request
                    .messages
                    .iter()
                    .find(|message| {
                        message.role == Role::Assistant
                            && serde_json::from_str::<Value>(&message.content)
                                .is_ok_and(|value| value["event"] == "generation_output_limit")
                    })
                    .expect("generation recovery hint must be delivered");
                if matches!(reply, Reply::ChangedIdEcho) {
                    let mut value: Value = serde_json::from_str(&hint.content).unwrap();
                    value["data"]["trigger"] = json!("attempt_wrong_id");
                    value["data"]["scope"] = json!("attempt_wrong_id");
                    ScriptedTurn::text(&value.to_string())
                } else {
                    ScriptedTurn::text(&hint.content)
                }
            }
            Reply::ObservationEcho(event) => {
                let hint = request
                    .messages
                    .iter()
                    .find(|message| {
                        message.role == Role::Assistant
                            && serde_json::from_str::<Value>(&message.content)
                                .is_ok_and(|value| value["event"] == event)
                    })
                    .expect("runtime observation must be delivered");
                ScriptedTurn::text(&hint.content)
            }
            Reply::BoundaryEcho => {
                let boundary = request.messages.last().unwrap();
                assert_eq!(boundary.role, Role::User);
                ScriptedTurn::text(&boundary.content)
            }
            Reply::Empty => ScriptedTurn::text(" \n\t"),
            Reply::PrefillSensitive(next) => {
                if let Some(message) = request.messages.last()
                    && message.role == Role::Assistant
                    && message.tool_calls.is_empty()
                {
                    ScriptedTurn::text(&message.content)
                } else {
                    next
                }
            }
            Reply::Pending => return Ok(Box::pin(futures_util::stream::pending())),
            Reply::NeedsBudget(required, turn) => {
                if request.max_output_tokens >= required {
                    turn
                } else {
                    self.replies
                        .lock()
                        .unwrap()
                        .push_front(Reply::NeedsBudget(required, turn));
                    ScriptedTurn {
                        deltas: vec![
                            ModelDelta::Tool {
                                index: 0,
                                id: "oversized".into(),
                                name: "files.write".into(),
                                arguments: "{\"path\":\"never-partial.txt\",\"content\":\"".into(),
                            },
                            ModelDelta::Finish {
                                reason: "length".into(),
                            },
                            ModelDelta::Done,
                        ],
                        delay_ms: 0,
                        error: None,
                    }
                }
            }
        };
        Ok(Box::pin(futures_util::stream::iter(
            turn.deltas.into_iter().map(Ok),
        )))
    }
}

fn write(path: &str, content: &str) -> Reply {
    Reply::Turn(ScriptedTurn::calls(
        "",
        vec![ToolCall {
            id: id("call"),
            name: "files.write".into(),
            arguments: json!({"path":path,"content":content}),
        }],
    ))
}
fn limited() -> Reply {
    Reply::Turn(ScriptedTurn {
        deltas: vec![
            ModelDelta::Tool {
                index: 0,
                id: "discarded-complete".into(),
                name: "files.write".into(),
                arguments: json!({"path":"discarded.txt","content":"must not execute"}).to_string(),
            },
            ModelDelta::Tool {
                index: 1,
                id: "discarded-partial".into(),
                name: "files.write".into(),
                arguments: r#"{"path":"partial.txt","content":"unfinished"#.into(),
            },
            ModelDelta::Finish {
                reason: "length".into(),
            },
            ModelDelta::Done,
        ],
        delay_ms: 0,
        error: None,
    })
}
fn profile() -> AgentProfile {
    AgentProfile {
        schema_version: 1,
        name: "recovery".into(),
        prompt: "Implement the requested changes and report truthfully.".into(),
        tools: vec!["files.write".into()],
        skills: vec![],
    }
}
fn policy() -> HostPolicy {
    HostPolicy {
        workspace_writable: true,
        grants: vec![CapabilityGrant {
            tool: "files.write".into(),
            version: "1".into(),
            resource: ".".into(),
            arguments_hash: None,
            expires_at_ms: None,
            preapproved: true,
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn recovery_grows_the_actual_budget_within_provider_and_host_caps() {
    for (provider_cap, host_cap, expected) in [
        (Some(8192), None, 8192),
        (Some(8192), Some(6144), 6144),
        (None, Some(6144), 6144),
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let mut provider = RecoveryProvider::new(vec![
            Reply::NeedsBudget(
                expected,
                ScriptedTurn::calls(
                    "",
                    vec![ToolCall {
                        id: "complete".into(),
                        name: "files.write".into(),
                        arguments: json!({"path":"complete.js","content":"function game() {}"}),
                    }],
                ),
            ),
            Reply::Turn(ScriptedTurn::text("Implemented.")),
        ]);
        provider.output_ceiling = provider_cap;
        let provider = Arc::new(provider);
        let engine = WorkerEngine::builder(provider.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(), workspace.path(), policy(), Value::Null)
            .await
            .unwrap();
        let limits = Limits {
            context_tokens: 32768,
            reserve_output_tokens: 4096,
            max_output_tokens: host_cap,
            ..Default::default()
        };
        let run = session
            .submit("Implement game", "start", limits, None)
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
        assert_eq!(
            provider
                .requests()
                .iter()
                .map(|r| r.max_output_tokens)
                .collect::<Vec<_>>(),
            vec![4096, expected, expected]
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("complete.js")).unwrap(),
            "function game() {}"
        );
        assert!(!workspace.path().join("never-partial.txt").exists());
        let state = run.status().await.unwrap();
        let restored: Run = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        assert_eq!(
            restored.generation_recovery.target_output_tokens,
            Some(expected)
        );
        validate_messages(&session.snapshot().await.unwrap().messages).unwrap();
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn exhausted_budget_bounds_write_schema_and_rejects_oversized_complete_calls() {
    let workspace = tempfile::tempdir().unwrap();
    let provider = Arc::new(RecoveryProvider::new(vec![
        limited(),
        write("too-large.js", &"x".repeat(2049)),
        write("small.js", "function game() {}"),
        Reply::Turn(ScriptedTurn::text("Done")),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(profile(), workspace.path(), policy(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("Implement", "start", Limits::default(), None)
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert!(!workspace.path().join("too-large.js").exists());
    assert!(workspace.path().join("small.js").exists());
    let requests = provider.requests();
    assert_eq!(
        requests[0].tools[0].input_schema["properties"]["content"]["maxLength"],
        65536
    );
    assert_eq!(
        requests[1].tools[0].input_schema["properties"]["content"]["maxLength"],
        2048
    );
    assert_eq!(
        run.status().await.unwrap().invocations[0]
            .result
            .as_ref()
            .unwrap()
            .status,
        Outcome::ValidationError
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn grown_output_borrows_only_free_context_and_keeps_user_constraints() {
    let workspace = tempfile::tempdir().unwrap();
    let mut provider =
        RecoveryProvider::new(vec![limited(), Reply::Turn(ScriptedTurn::text("Done"))]);
    provider.output_ceiling = Some(16383);
    let provider = Arc::new(provider);
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(profile(), workspace.path(), policy(), Value::Null)
        .await
        .unwrap();
    let input = format!("Keep this complete constraint: {}", "x".repeat(2000));
    let run = session
        .submit(
            &input,
            "start",
            Limits {
                context_tokens: 8192,
                reserve_output_tokens: 2048,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    let requests = provider.requests();
    assert!(requests[1].max_output_tokens < 4096);
    assert!(requests[1].max_output_tokens >= 2048);
    assert_eq!(
        requests[1].tools[0].input_schema["properties"]["content"]["maxLength"],
        2048
    );
    assert_eq!(
        requests.len(),
        2,
        "switching schemas must not spend another inference"
    );
    for request in &requests {
        assert!(
            werk_worker::context::estimate_request(request) + request.max_output_tokens + 512
                <= 8192
        );
        assert!(
            request
                .messages
                .iter()
                .any(|message| message.role == Role::User && message.content == input)
        );
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn generation_limits_after_reads_keep_fresh_feedback_and_shrink_until_a_write() {
    let workspace = tempfile::tempdir().unwrap();
    let mut replies = vec![write("saved.txt", "prior success")];
    for n in 0..4 {
        std::fs::write(
            workspace.path().join(format!("observation-{n}.txt")),
            "evidence",
        )
        .unwrap();
        replies.push(limited());
        if n < 3 {
            replies.push(Reply::Turn(ScriptedTurn::calls(
                "",
                vec![ToolCall {
                    id: id("read"),
                    name: "files.read".into(),
                    arguments: json!({"path":format!("observation-{n}.txt")}),
                }],
            )));
        }
    }
    replies.extend([
        write("part-one.js", "function first() {}"),
        limited(),
        write("part-two.js", "function second() {}"),
        Reply::Turn(ScriptedTurn::text("Done")),
    ]);
    let provider = Arc::new(RecoveryProvider::new(replies));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let mut agent = profile();
    agent.tools.push("files.read".into());
    let mut access = policy();
    access.grants.push(CapabilityGrant {
        tool: "files.read".into(),
        version: "1".into(),
        resource: ".".into(),
        arguments_hash: None,
        expires_at_ms: None,
        preapproved: true,
    });
    let session = engine
        .create_session(agent, workspace.path(), access, Value::Null)
        .await
        .unwrap();
    let run = session
        .submit(
            "Implement the game in small complete parts",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    let requests = provider.requests();
    let saved = session.snapshot().await.unwrap();
    let attempts = &saved.run(&run.id).unwrap().attempts;
    for (failed_index, chars) in [(1, 1024), (3, 512), (5, 256), (7, 256), (9, 1024)] {
        let hint: Value = requests[failed_index + 1]
            .messages
            .iter()
            .filter_map(|m| serde_json::from_str::<Value>(&m.content).ok())
            .find(|v| v["event"] == "generation_output_limit")
            .expect(
                "every new generation failure must bypass observation cooldown and episode caps",
            );
        assert_eq!(hint["data"]["trigger"], attempts[failed_index].attempt_id);
        assert!(
            hint["data"]["detail"]
                .as_str()
                .unwrap()
                .contains(&format!("at most {chars} characters")),
            "{hint}"
        );
        assert_eq!(requests[failed_index + 1].max_output_tokens, 2048);
    }
    assert!(!workspace.path().join("partial.txt").exists());
    assert!(!workspace.path().join("discarded.txt").exists());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("part-one.js")).unwrap(),
        "function first() {}"
    );
    assert_eq!(
        saved.metadata["_werkworker_harness"][&run.id]["adaptive"]["aids"]["generation_output_limit"]
            ["episodes"],
        5
    );
    for request in requests {
        validate_messages(&request.messages).unwrap();
    }
    engine.close().await.unwrap();
}

struct FixedRecoveryContext {
    change_output: bool,
}
#[async_trait]
impl werk_worker::context::ContextBuilder for FixedRecoveryContext {
    async fn build(
        &self,
        _: &SessionData,
        run: &Run,
        _tools: Vec<ToolDefinition>,
    ) -> Result<(ModelRequest, ContextSnapshot)> {
        Ok((
            ModelRequest {
                reasoning_tier: None,
                messages: vec![Message::text(Role::User, "Implement")],
                // Deliberately ignore engine recovery changes: the replay guard
                // must still reject an identical custom-builder request.
                tools: vec![],
                max_output_tokens: if self.change_output && !run.attempts.is_empty() {
                    1024
                } else {
                    2048
                },
                output_schema: None,
            },
            ContextSnapshot {
                id: id("ctx"),
                hash: "custom-fixed-context-hash".into(),
                source: None,
                planning: None,
                estimated_tokens: 8,
                message_count: 1,
                omitted_messages: vec![],
                summary: None,
            },
        ))
    }
}

#[tokio::test]
async fn identical_truncated_request_is_blocked_durably_but_changed_output_is_allowed() {
    for change_output in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::default());
        let provider = Arc::new(RecoveryProvider::new(vec![
            limited(),
            Reply::Turn(ScriptedTurn::text("Done")),
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .store(store.clone())
            .context(Arc::new(FixedRecoveryContext { change_output }))
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(), workspace.path(), policy(), Value::Null)
            .await
            .unwrap();
        let run = session
            .submit("Implement", "start", Limits::default(), None)
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        if change_output {
            assert_eq!(result.status, RunStatus::Completed);
            assert_eq!(provider.requests().len(), 2);
        } else {
            assert_eq!(result.error.unwrap().code, "model_output_recovery_stalled");
            assert_eq!(provider.requests().len(), 1);
            let mut saved = session.snapshot().await.unwrap();
            engine.close().await.unwrap();
            let revision = saved.revision;
            let state = saved.runs.last_mut().unwrap();
            state.status = RunStatus::Running;
            state.error = None;
            saved.revision += 1;
            store.save(&saved, Some(revision)).await.unwrap();
            let next = Arc::new(RecoveryProvider::new(vec![]));
            let reopened = WorkerEngine::builder(next.clone())
                .store(store)
                .context(Arc::new(FixedRecoveryContext {
                    change_output: false,
                }))
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
            assert_eq!(result.error.unwrap().code, "model_output_recovery_stalled");
            assert!(next.requests().is_empty());
            reopened.close().await.unwrap();
            continue;
        }
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn output_limit_then_empty_or_feedback_echo_recovers_without_false_completion() {
    for bad_reply in [Reply::Empty, Reply::FeedbackEcho, Reply::BoundaryEcho] {
        let workspace = tempfile::tempdir().unwrap();
        let provider = Arc::new(RecoveryProvider::new(vec![
            write("saved.txt", "prior success"),
            limited(),
            bad_reply,
            write("finished.txt", "complete implementation"),
            Reply::Turn(ScriptedTurn::text("Implemented and verified.")),
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(), workspace.path(), policy(), Value::Null)
            .await
            .unwrap();
        let run = session
            .submit("Implement", "start", Limits::default(), None)
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.text.as_deref(), Some("Implemented and verified."));
        assert_eq!(result.metrics.model_requests, 5);
        assert_eq!(result.metrics.tool_invocations, 2);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("saved.txt")).unwrap(),
            "prior success"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("finished.txt")).unwrap(),
            "complete implementation"
        );
        assert!(!workspace.path().join("discarded.txt").exists());
        assert!(!workspace.path().join("partial.txt").exists());
        let data = session.snapshot().await.unwrap();
        validate_messages(&data.messages).unwrap();
        let state = data.run(&run.id).unwrap();
        assert_eq!(
            state
                .attempts
                .iter()
                .filter(|attempt| !attempt.completed)
                .count(),
            2
        );
        assert!(
            state.attempts[2]
                .error
                .as_ref()
                .is_some_and(|error| matches!(
                    error.code.as_str(),
                    "model_empty_response" | "model_feedback_echo"
                ))
        );
        let requests = provider.requests();
        // Invalid continuations retry the same admitted request, including the
        // one-shot recovery evidence and the fresh-turn boundary.
        assert_eq!(
            serde_json::to_value(&requests[2]).unwrap(),
            serde_json::to_value(&requests[3]).unwrap()
        );
        assert_eq!(requests[2].messages.last().unwrap().role, Role::User);
        for request in requests {
            validate_messages(&request.messages).unwrap();
        }
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn each_new_generation_limit_updates_the_same_bounded_recovery_episode() {
    for completes in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let mut replies = vec![write("saved.txt", "prior success"), limited(), limited()];
        if completes {
            replies.extend([
                write("finished.txt", "complete implementation"),
                Reply::Turn(ScriptedTurn::text("Implemented.")),
            ]);
        } else {
            replies.push(limited());
        }
        let provider = Arc::new(RecoveryProvider::new(replies));
        let engine = WorkerEngine::builder(provider.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(), workspace.path(), policy(), Value::Null)
            .await
            .unwrap();
        let run = session
            .submit("Implement", "start", Limits::default(), None)
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(
            result.status,
            if completes {
                RunStatus::Completed
            } else {
                RunStatus::Failed
            }
        );
        let saved = session.snapshot().await.unwrap();
        let state = saved.run(&run.id).unwrap();
        let requests = provider.requests();
        assert_eq!(requests.len(), if completes { 5 } else { 4 });
        for (request_index, failed_index) in [(2, 1), (3, 2)] {
            let observations: Vec<Value> = requests[request_index]
                .messages
                .iter()
                .filter_map(|m| serde_json::from_str::<Value>(&m.content).ok())
                .filter(|v| v["event"] == "generation_output_limit")
                .collect();
            assert_eq!(observations.len(), 1);
            assert_eq!(
                observations[0]["data"]["trigger"],
                state.attempts[failed_index].attempt_id
            );
            assert_eq!(
                observations[0]["data"]["scope"],
                state.attempts[failed_index].attempt_id
            );
            assert_eq!(
                requests[request_index].messages.last().unwrap().role,
                Role::User
            );
            validate_messages(&requests[request_index].messages).unwrap();
        }
        assert_eq!(
            saved.metadata["_werkworker_harness"][&run.id]["adaptive"]["aids"]["generation_output_limit"]
                ["episodes"],
            1
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("saved.txt")).unwrap(),
            "prior success"
        );
        assert_eq!(workspace.path().join("finished.txt").exists(), completes);
        assert!(!workspace.path().join("discarded.txt").exists());
        assert!(!workspace.path().join("partial.txt").exists());
        if !completes {
            assert_eq!(result.error.unwrap().code, "model_output_limit");
        }
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn resumed_repeated_limit_keeps_latest_evidence_and_spent_allowance() {
    for old_suppressed_snapshot in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::default());
        let provider = Arc::new(RecoveryProvider::new(vec![
            limited(),
            limited(),
            Reply::Pending,
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .store(store.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(), workspace.path(), policy(), Value::Null)
            .await
            .unwrap();
        let run = session
            .submit("Implement", "start", Limits::default(), None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while provider.requests().len() < 3 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut saved = session.snapshot().await.unwrap();
        let latest = saved.runs[0].attempts[1].attempt_id.clone();
        if old_suppressed_snapshot {
            // The prior implementation recorded the second failure as observed
            // but left the already delivered aid pointing at the first one.
            let old = provider.requests()[1]
                .messages
                .iter()
                .filter_map(|m| serde_json::from_str::<Value>(&m.content).ok())
                .find(|v| v["event"] == "generation_output_limit")
                .unwrap();
            let aid = &mut saved.metadata["_werkworker_harness"][&run.id]["adaptive"]["aids"]["generation_output_limit"];
            aid["trigger"] = old["data"]["trigger"].clone();
            aid["scope"] = old["data"]["scope"].clone();
            aid["detail"] = old["data"]["detail"].clone();
            aid["delivered_step"] = json!(saved.runs[0].attempts[1].step_id);
        }
        engine.close().await.unwrap();
        let revision = store.load(&saved.id).await.unwrap().revision;
        saved.revision = revision + 1;
        store.save(&saved, Some(revision)).await.unwrap();
        let next = Arc::new(RecoveryProvider::new(vec![limited()]));
        let reopened = WorkerEngine::builder(next.clone())
            .store(store)
            .build()
            .await
            .unwrap();
        let resumed = reopened.resume_run(&saved.id, &run.id).await.unwrap();
        let result = resumed.wait().await.unwrap();
        assert_eq!(result.status, RunStatus::Failed);
        assert_eq!(result.error.unwrap().code, "model_output_limit");
        let requests = next.requests();
        assert_eq!(requests.len(), 1, "resume must not replenish retries");
        let notice = requests[0]
            .messages
            .iter()
            .filter_map(|m| serde_json::from_str::<Value>(&m.content).ok())
            .find(|v| v["event"] == "generation_output_limit")
            .unwrap();
        assert_eq!(notice["data"]["trigger"], latest);
        let data = reopened
            .open_session(&saved.id)
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap();
        assert_eq!(
            data.metadata["_werkworker_harness"][&run.id]["adaptive"]["aids"]["generation_output_limit"]
                ["episodes"],
            1
        );
        assert!(data.runs[0].invocations.is_empty());
        reopened.close().await.unwrap();
    }
}

#[tokio::test]
async fn repeated_empty_and_echo_responses_share_the_output_recovery_budget() {
    for echo in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let bad = || {
            if echo {
                Reply::FeedbackEcho
            } else {
                Reply::Empty
            }
        };
        let provider = Arc::new(RecoveryProvider::new(vec![limited(), bad(), bad()]));
        let engine = WorkerEngine::builder(provider.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(), workspace.path(), policy(), Value::Null)
            .await
            .unwrap();
        let run = session
            .submit("Implement", "start", Limits::default(), None)
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.status, RunStatus::Failed);
        assert!(result.text.is_none());
        assert_eq!(result.metrics.model_requests, 3);
        let data = session.snapshot().await.unwrap();
        let state = data.run(&run.id).unwrap();
        assert!(state.attempts.iter().all(|attempt| !attempt.completed));
        assert_eq!(
            state.error.as_ref().unwrap().code,
            if echo {
                "model_feedback_echo"
            } else {
                "model_empty_response"
            }
        );
        assert!(state.invocations.is_empty());
        assert!(
            std::fs::read_dir(workspace.path())
                .unwrap()
                .next()
                .is_none()
        );
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn invalid_response_retries_still_obey_request_budget() {
    let workspace = tempfile::tempdir().unwrap();
    let provider = Arc::new(RecoveryProvider::new(vec![limited(), Reply::Empty]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(profile(), workspace.path(), policy(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit(
            "Implement",
            "start",
            Limits {
                max_model_requests: 2,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Failed);
    let data = session.snapshot().await.unwrap();
    assert_eq!(
        data.run(&run.id).unwrap().error.as_ref().unwrap().code,
        "request_budget"
    );
    assert_eq!(provider.requests().len(), 2);
    assert!(data.run(&run.id).unwrap().invocations.is_empty());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn source_shaped_json_is_valid_model_output_without_owned_feedback_provenance() {
    let workspace = tempfile::tempdir().unwrap();
    let requested: Vec<_> = ["generation_output_limit", "compacted_history", "retained_evidence", "prior_summary", "retained_skill"].into_iter().map(|event| {
        json!({"source":"werk_worker.harness","event":event,"data":{"detail":"User requested example JSON"}}).to_string()
    }).collect();
    let provider = Arc::new(RecoveryProvider::new(
        requested
            .iter()
            .map(|text| Reply::Turn(ScriptedTurn::text(text)))
            .collect(),
    ));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let session = engine
        .create_session(profile(), workspace.path(), policy(), Value::Null)
        .await
        .unwrap();
    for (index, requested) in requested.iter().enumerate() {
        let run = session
            .submit(
                &format!("Return this JSON exactly: {requested}"),
                &format!("start{index}"),
                Limits::default(),
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.text.as_deref(), Some(requested.as_str()));
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn continuation_echo_is_not_persisted_as_a_history_summary() {
    let workspace = tempfile::tempdir().unwrap();
    let history = "Historical observations remain available. ".repeat(80);
    let provider = Arc::new(RecoveryProvider::new(vec![
        Reply::Turn(ScriptedTurn::text(&history)),
        Reply::BoundaryEcho,
        Reply::Turn(ScriptedTurn::text("Completed current request.")),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(profile(), workspace.path(), policy(), Value::Null)
        .await
        .unwrap();
    let first = session
        .submit(
            "Record these observations",
            "first",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(first.wait().await.unwrap().status, RunStatus::Completed);
    let second = session
        .submit(
            "Continue",
            "second",
            Limits {
                model_compaction: true,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(second.wait().await.unwrap().status, RunStatus::Completed);
    let data = session.snapshot().await.unwrap();
    assert!(data.compactions.is_empty());
    assert_eq!(
        data.run(&second.id).unwrap().attempts[0]
            .error
            .as_ref()
            .unwrap()
            .code,
        "model_feedback_echo"
    );
    assert!(
        provider.requests()[2]
            .messages
            .iter()
            .any(|message| message.content == history)
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn resume_preserves_consumed_retries_with_durable_schema_change() {
    let workspace = tempfile::tempdir().unwrap();
    let store = Arc::new(InMemoryStore::default());
    let provider = Arc::new(RecoveryProvider::new(vec![
        limited(),
        Reply::Empty,
        Reply::Pending,
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .store(store.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(profile(), workspace.path(), policy(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("Implement", "start", Limits::default(), None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while provider.requests().len() < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut saved = session.snapshot().await.unwrap();
    assert_eq!(
        saved
            .run(&run.id)
            .unwrap()
            .attempts
            .iter()
            .filter(|attempt| attempt.error.is_some())
            .count(),
        2
    );
    engine.close().await.unwrap();
    let revision = store.load(&saved.id).await.unwrap().revision;
    saved.revision = revision + 1;
    store.save(&saved, Some(revision)).await.unwrap();
    let next = Arc::new(RecoveryProvider::new(vec![Reply::Empty]));
    let reopened = WorkerEngine::builder(next.clone())
        .store(store)
        .build()
        .await
        .unwrap();
    let resumed = reopened.resume_run(&saved.id, &run.id).await.unwrap();
    assert_eq!(resumed.wait().await.unwrap().status, RunStatus::Failed);
    assert_eq!(next.requests().len(), 1);
    assert_eq!(
        next.requests()[0].tools[0].input_schema["properties"]["content"]["maxLength"],
        2048
    );
    let data = reopened
        .open_session(&saved.id)
        .await
        .unwrap()
        .snapshot()
        .await
        .unwrap();
    let state = data.run(&run.id).unwrap();
    assert_eq!(state.error.as_ref().unwrap().code, "model_empty_response");
    assert_eq!(state.metrics.model_requests, 4);
    assert!(state.invocations.is_empty());
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn legacy_minimal_hints_and_boundary_echoes_are_rejected_after_resume() {
    for echo in [
        Reply::ObservationEcho("output_limit"),
        Reply::ObservationEcho("budget_low"),
        Reply::BoundaryEcho,
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::default());
        let provider = Arc::new(RecoveryProvider::new(vec![Reply::Pending]));
        let engine = WorkerEngine::builder(provider.clone())
            .store(store.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(), workspace.path(), policy(), Value::Null)
            .await
            .unwrap();
        let run = session
            .submit(
                "Implement",
                "start",
                Limits {
                    max_steps: 5,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while provider.requests().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut saved = session.snapshot().await.unwrap();
        engine.close().await.unwrap();
        // A historical run keeps its minimal contract when resumed, even
        // though newly submitted runs normalize to the adaptive default.
        saved.metadata["_werkworker_harness"][&run.id]["config"]["mode"] = json!("minimal");
        let revision = store.load(&saved.id).await.unwrap().revision;
        saved.revision = revision + 1;
        store.save(&saved, Some(revision)).await.unwrap();
        let next = Arc::new(RecoveryProvider::new(vec![
            limited(),
            echo,
            write("finished.txt", "completed"),
            Reply::Turn(ScriptedTurn::text("Implemented.")),
        ]));
        let reopened = WorkerEngine::builder(next.clone())
            .store(store)
            .build()
            .await
            .unwrap();
        let resumed = reopened.resume_run(&saved.id, &run.id).await.unwrap();
        let result = resumed.wait().await.unwrap();
        assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
        assert_eq!(result.text.as_deref(), Some("Implemented."));
        assert_eq!(next.requests().len(), 4);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("finished.txt")).unwrap(),
            "completed"
        );
        assert!(!workspace.path().join("discarded.txt").exists());
        let data = reopened
            .open_session(&saved.id)
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap();
        assert_eq!(
            harness::config(&data, data.run(&run.id).unwrap())
                .unwrap()
                .mode,
            harness::HarnessMode::Minimal
        );
        assert!(data.run(&run.id).unwrap().attempts.iter().any(|attempt| {
            attempt
                .error
                .as_ref()
                .is_some_and(|error| error.code == "model_feedback_echo")
        }));
        // The failed echo is retried with precisely the same admitted notice;
        // decrementing counters for a new request must not alter its ownership.
        assert_eq!(
            serde_json::to_value(&next.requests()[1]).unwrap(),
            serde_json::to_value(&next.requests()[2]).unwrap()
        );
        reopened.close().await.unwrap();
    }
}

#[tokio::test]
async fn compacted_successful_write_narrative_starts_a_fresh_response() {
    let workspace = tempfile::tempdir().unwrap();
    let content = "const gameState = {};\n".repeat(800);
    let narrative = "The initial code is saved; I still need to finish the task.";
    let provider = Arc::new(RecoveryProvider::new(vec![
        Reply::Turn(ScriptedTurn::calls(
            narrative,
            vec![ToolCall {
                id: id("call"),
                name: "files.write".into(),
                arguments: json!({"path":"game.js","content":content}),
            }],
        )),
        Reply::BoundaryEcho,
        Reply::PrefillSensitive(ScriptedTurn::text(
            "Task completed after reviewing the saved code.",
        )),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(profile(), workspace.path(), policy(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit(
            "Implement the task",
            "start",
            Limits {
                context_tokens: 8192,
                reserve_output_tokens: 2048,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(
        result.text.as_deref(),
        Some("Task completed after reviewing the saved code.")
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("game.js")).unwrap(),
        content
    );
    let data = session.snapshot().await.unwrap();
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(
        data.run(&run.id).unwrap().attempts[1]
            .error
            .as_ref()
            .unwrap()
            .code,
        "model_feedback_echo"
    );
    assert!(
        !data.run(&run.id).unwrap().contexts[1]
            .omitted_messages
            .is_empty()
    );
    let request = &provider.requests()[1];
    assert!(
        request
            .messages
            .iter()
            .any(|message| message.content == narrative && message.tool_calls.is_empty())
    );
    assert_eq!(request.messages.last().unwrap().role, Role::User);
    validate_messages(&request.messages).unwrap();
    engine.close().await.unwrap();
}

#[tokio::test]
async fn compaction_observation_echoes_are_retried_without_polluting_history() {
    for event in ["compacted_history", "retained_evidence"] {
        let workspace = tempfile::tempdir().unwrap();
        let content = "const gameState = {};\n".repeat(800);
        let provider = Arc::new(RecoveryProvider::new(vec![
            write("game.js", &content),
            Reply::ObservationEcho(event),
            write("finished.txt", "continued implementation"),
            Reply::Turn(ScriptedTurn::text("Implementation continued.")),
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile(), workspace.path(), policy(), Value::Null)
            .await
            .unwrap();
        let run = session
            .submit(
                "Implement the game",
                "start",
                Limits {
                    context_tokens: 8192,
                    reserve_output_tokens: 2048,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(
            result.status,
            RunStatus::Completed,
            "{event}: {:?}",
            result.error
        );
        assert_eq!(result.text.as_deref(), Some("Implementation continued."));
        let data = session.snapshot().await.unwrap();
        let state = data.run(&run.id).unwrap();
        assert_eq!(
            state.attempts[1].error.as_ref().map(|e| e.code.as_str()),
            Some("model_feedback_echo")
        );
        assert!(!state.attempts[1].completed);
        assert!(!state.contexts[1].omitted_messages.is_empty());
        let requests = provider.requests();
        assert_eq!(
            serde_json::to_value(&requests[1]).unwrap(),
            serde_json::to_value(&requests[2]).unwrap()
        );
        assert!(
            !data
                .messages
                .iter()
                .any(|m| serde_json::from_str::<Value>(&m.content)
                    .is_ok_and(|v| v["event"] == event))
        );
        assert!(state.contexts.iter().all(|c| {
            c.source
                .as_ref()
                .is_none_or(|s| !s.excerpt.contains("werk_worker.harness"))
        }));
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("game.js")).unwrap(),
            content
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("finished.txt")).unwrap(),
            "continued implementation"
        );
        validate_messages(&data.messages).unwrap();
        for request in &requests {
            validate_messages(&request.messages).unwrap();
        }
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn prior_summary_and_restored_skill_echoes_are_retried_on_followup() {
    for event in ["prior_summary", "retained_skill"] {
        let workspace = tempfile::tempdir().unwrap();
        let provider = Arc::new(RecoveryProvider::new(vec![
            Reply::Turn(ScriptedTurn::calls(
                "",
                vec![ToolCall {
                    id: id("call"),
                    name: "skills.load".into(),
                    arguments: json!({"name":"document-analysis"}),
                }],
            )),
            Reply::Turn(ScriptedTurn::text("Skill reviewed.")),
            Reply::Turn(ScriptedTurn::text(
                "The document-analysis skill was loaded successfully.",
            )),
            Reply::ObservationEcho(event),
            Reply::Turn(ScriptedTurn::text("Follow-up completed.")),
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .build()
            .await
            .unwrap();
        let mut agent = profile();
        agent.tools = vec!["skills.load".into()];
        agent.skills = vec!["document-analysis".into()];
        let session = engine
            .create_session(agent, workspace.path(), HostPolicy::default(), Value::Null)
            .await
            .unwrap();
        let first = session
            .submit("Review the skill", "first", Limits::default(), None)
            .await
            .unwrap();
        assert_eq!(first.wait().await.unwrap().status, RunStatus::Completed);
        let second = session
            .submit(
                "Continue",
                "second",
                Limits {
                    model_compaction: true,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let result = second.wait().await.unwrap();
        assert_eq!(
            result.status,
            RunStatus::Completed,
            "{event}: {:?}",
            result.error
        );
        assert_eq!(result.text.as_deref(), Some("Follow-up completed."));
        let data = session.snapshot().await.unwrap();
        assert_eq!(data.compactions.len(), 1);
        assert_eq!(
            data.run(&second.id).unwrap().attempts[1]
                .error
                .as_ref()
                .map(|e| e.code.as_str()),
            Some("model_feedback_echo")
        );
        assert!(
            !data
                .messages
                .iter()
                .any(|m| serde_json::from_str::<Value>(&m.content)
                    .is_ok_and(|v| v["event"] == event))
        );
        let requests = provider.requests();
        assert_eq!(
            serde_json::to_value(&requests[3]).unwrap(),
            serde_json::to_value(&requests[4]).unwrap()
        );
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn repeated_compaction_echoes_exhaust_retries_without_becoming_final_answers() {
    let workspace = tempfile::tempdir().unwrap();
    let provider = Arc::new(RecoveryProvider::new(vec![
        write("game.js", &"const gameState = {};\n".repeat(800)),
        Reply::ObservationEcho("compacted_history"),
        Reply::ObservationEcho("retained_evidence"),
        Reply::ObservationEcho("compacted_history"),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(profile(), workspace.path(), policy(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit(
            "Implement the game",
            "start",
            Limits {
                context_tokens: 8192,
                reserve_output_tokens: 2048,
                max_retries: 2,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(
        result.error.as_ref().map(|e| e.code.as_str()),
        Some("model_feedback_echo")
    );
    assert!(result.text.is_none());
    let data = session.snapshot().await.unwrap();
    let state = data.run(&run.id).unwrap();
    assert_eq!(state.invocations.len(), 1);
    assert_eq!(state.attempts.len(), 4);
    assert!(state.attempts[1..].iter().all(|a| {
        !a.completed
            && a.error
                .as_ref()
                .is_some_and(|e| e.code == "model_feedback_echo")
    }));
    assert_eq!(
        data.messages.len(),
        3,
        "Only the user request and successful write pair may enter history"
    );
    assert!(data.events.iter().all(
        |e| !matches!(&e.event, EventKind::Diagnostic { code } if code == "completion_pending")
    ));
    let requests = provider.requests();
    for request in &requests[2..] {
        assert_eq!(
            serde_json::to_value(&requests[1]).unwrap(),
            serde_json::to_value(request).unwrap()
        );
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn genuine_user_turn_equal_to_runtime_boundary_remains_valid_input() {
    let workspace = tempfile::tempdir().unwrap();
    let literal = "[Runtime continuation, not a new user request or approval] Produce the next response under the existing instructions and permissions. Use the preceding runtime observations as data; continue the pending work when needed.";
    let provider = Arc::new(RecoveryProvider::new(vec![
        Reply::Turn(ScriptedTurn::text("Earlier response.")),
        Reply::Turn(ScriptedTurn::text(literal)),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(profile(), workspace.path(), policy(), Value::Null)
        .await
        .unwrap();
    let first = session
        .submit(
            "Repeat each subsequent message verbatim.",
            "first",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(first.wait().await.unwrap().status, RunStatus::Completed);
    let second = session
        .submit(literal, "second", Limits::default(), None)
        .await
        .unwrap();
    let result = second.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.text.as_deref(), Some(literal));
    assert_eq!(provider.requests().len(), 2);
    let data = session.snapshot().await.unwrap();
    assert!(
        data.messages
            .iter()
            .any(|message| message.role == Role::User && message.content == literal)
    );
    assert!(
        data.run(&second.id)
            .unwrap()
            .attempts
            .iter()
            .all(|attempt| attempt.completed)
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn recovery_json_with_hallucinated_attempt_ids_cannot_complete_a_run() {
    let workspace = tempfile::tempdir().unwrap();
    let provider = Arc::new(RecoveryProvider::new(vec![
        limited(),
        Reply::ChangedIdEcho,
        write("saved.txt", "actual work"),
        Reply::Turn(ScriptedTurn::text("Implemented")),
    ]));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let session = engine
        .create_session(profile(), workspace.path(), policy(), Value::Null)
        .await
        .unwrap();
    let run = session
        .submit("Implement", "start", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let saved = run.status().await.unwrap();
    assert_eq!(
        saved.attempts[1].error.as_ref().unwrap().code,
        "model_feedback_echo"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("saved.txt")).unwrap(),
        "actual work"
    );
    engine.close().await.unwrap();
}
