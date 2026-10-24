// Infrastructure fixtures explicitly opt out of coding acceptance checks.
// Behavioral completion is exercised in tests/completion.rs.
#![cfg(target_os = "linux")]
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use werk_worker::{
    context::{ContextBuilder, DefaultContextBuilder, estimate_request, validate_messages},
    harness::HarnessConfig,
    policy::{HostPolicy, ToolPermission},
    project::{CONFIG, Project},
    providers::{ModelRequest, ScriptedProvider, ScriptedTurn},
    sessions::{InMemoryStore, SessionStore, SqliteStore},
    *,
};

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
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
fn policy() -> HostPolicy {
    HostPolicy {
        workspace_writable: true,
        tool_permissions: ["files.read", "files.write", "files.edit", "process.run"]
            .into_iter()
            .map(|s| (s.into(), ToolPermission::Allow))
            .collect(),
        ..Default::default()
    }
}
async fn project(root: &Path) -> Project {
    write(
        root,
        "AGENTS.md",
        "ROOT: preserve the user's existing work.",
    );
    write(
        root,
        "a/AGENTS.md",
        "ALPHA: use the local naming conventions.",
    );
    write(
        root,
        "a/deep/AGENTS.override.md",
        "DEEP: output must be corrected.txt; overrides ALPHA naming.",
    );
    write(
        root,
        "a/deep/AGENTS.md",
        "IGNORED sibling must not be loaded.",
    );
    write(root, "b/CLAUDE.md", "BETA: unrelated project rules.");
    write(
        root,
        CONFIG,
        r#"{"instruction_directories":["a/deep","b"]}"#,
    );
    Project::load(root).await.unwrap()
}
fn rules(request: &ModelRequest) -> Vec<Value> {
    request
        .messages
        .iter()
        .filter_map(|m| serde_json::from_str::<Value>(&m.content).ok())
        .filter(|v| v["source"] == "werk_worker.project")
        .collect()
}
fn assert_scoped(request: &ModelRequest) {
    let values = rules(request);
    assert_eq!(
        values
            .iter()
            .map(|v| v["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["AGENTS.md", "a/AGENTS.md", "a/deep/AGENTS.override.md"]
    );
    for v in values {
        assert_eq!(v["sha256"], hash(v["text"].as_str().unwrap()));
    }
    assert!(
        !request
            .messages
            .iter()
            .any(|m| m.content.contains("BETA: unrelated"))
    );
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.content.contains("HOST: preserve identifiers"))
    );
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.content == "USER: preserve public API")
    );
    validate_messages(&request.messages).unwrap();
}

#[tokio::test]
async fn scope_gate_reconfirmed_write_uses_one_repetition_with_and_without_resume() {
    for resume in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project(dir.path()).await;
        let profile = project
            .scoped_profile("coding-basic", dir.path(), global.path())
            .await
            .unwrap();
        let database = global.path().join("session.sqlite");
        let arguments = json!({"path":"a/deep/corrected.txt","content":"verified"});
        let confirmed = || {
            vec![
                call("files.write", arguments.clone()),
                ScriptedTurn::text("Saved the file."),
            ]
        };
        let mut turns = vec![call("files.write", arguments.clone())];
        if resume {
            let mut slow = ScriptedTurn::text("interrupted decision");
            slow.delay_ms = 60_000;
            turns.push(slow);
        } else {
            turns.extend(confirmed());
        }
        let provider = Arc::new(ScriptedProvider::new(turns));
        let mut engine = WorkerEngine::builder(provider.clone())
            .store(Arc::new(SqliteStore::open(&database).await.unwrap()))
            .harness(werk_worker::harness::HarnessConfig {
                coding_checks: false,
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(profile, dir.path(), policy(), project.scoped_metadata())
            .await
            .unwrap();
        let limits = Limits {
            max_repeated_calls: 1,
            max_tool_invocations: 2,
            max_model_requests: if resume { 4 } else { 3 },
            max_steps: if resume { 4 } else { 3 },
            ..Default::default()
        };
        let mut run = session
            .submit("Write the file", "start", limits.clone(), None)
            .await
            .unwrap();
        let session_id = session.id().await;
        let mut providers = vec![provider.clone()];
        let mut interrupted = None;
        if resume {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while provider.requests().len() < 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            assert!(!dir.path().join("a/deep/corrected.txt").exists());
            let mut saved = session.snapshot().await.unwrap();
            let state = saved.run(&run.id).unwrap();
            assert_eq!(state.metrics.tool_invocations, 1);
            assert_eq!(state.metrics.model_requests, 2);
            assert_eq!(state.metrics.steps, 2);
            interrupted = Some(state.clone());
            engine.close().await.unwrap();
            // Restore the durable interruption boundary, before orderly shutdown
            // records cancellation. Preserve all attempts and consumed budgets.
            let store = Arc::new(SqliteStore::open(&database).await.unwrap());
            let revision = store.load(&session_id).await.unwrap().revision;
            saved.revision = revision + 1;
            store.save(&saved, Some(revision)).await.unwrap();
            let next = Arc::new(ScriptedProvider::new(confirmed()));
            engine = WorkerEngine::builder(next.clone())
                .store(store)
                .harness(werk_worker::harness::HarnessConfig {
                    coding_checks: false,
                    ..Default::default()
                })
                .build()
                .await
                .unwrap();
            run = engine.resume_run(&session_id, &run.id).await.unwrap();
            providers.push(next);
        }
        let result = run.wait().await.unwrap();
        assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
        let saved = engine
            .open_session(&session_id)
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap();
        let state = saved.run(&run.id).unwrap();
        assert_eq!(serde_json::to_value(&state.limits).unwrap(), json!(limits));
        assert_eq!(state.metrics.tool_invocations, 2);
        assert_eq!(state.metrics.model_requests, limits.max_model_requests);
        assert_eq!(state.metrics.steps, limits.max_steps);
        assert_eq!(state.tree_model_requests, limits.max_model_requests);
        if let Some(before) = interrupted {
            assert_eq!(state.started_at_ms, before.started_at_ms);
            assert_eq!(state.deadline_ms, before.deadline_ms);
            assert_eq!(state.tree_model_requests, before.tree_model_requests + 2);
        }
        assert_eq!(state.invocations.len(), 2);
        let deferred = state.invocations[0].result.as_ref().unwrap();
        assert_eq!(deferred.status, Outcome::CancelledBeforeStart);
        assert_eq!(
            deferred.error_code.as_deref(),
            Some("project_instructions_required")
        );
        assert_eq!(
            state.invocations[0].call.arguments,
            state.invocations[1].call.arguments
        );
        assert_eq!(
            state.invocations[1].result.as_ref().unwrap().status,
            Outcome::Succeeded
        );
        assert_eq!(
            saved
                .events
                .iter()
                .filter(|e| matches!(e.event, EventKind::ToolStarted { .. }))
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/deep/corrected.txt")).unwrap(),
            "verified"
        );
        let requests: Vec<_> = providers.iter().flat_map(|p| p.requests()).collect();
        assert_eq!(rules(&requests[0]).len(), 1);
        for request in &requests[1..] {
            assert_eq!(rules(request).len(), 3);
            validate_messages(&request.messages).unwrap();
            assert!(!request.messages.iter().any(|m| {
                serde_json::from_str::<Value>(&m.content).is_ok_and(|v| {
                    v["source"] == "werk_worker.harness" && v["event"] == "repeated_action"
                })
            }));
        }
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn scope_gate_redecides_whole_batch_and_preserves_rules_across_compaction_resume_and_fork() {
    let dir = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    let project = project(dir.path()).await;
    let profile = project
        .scoped_profile("coding-basic", dir.path(), global.path())
        .await
        .unwrap();
    assert!(!profile.prompt.contains("ROOT:"));
    let store = Arc::new(InMemoryStore::default());
    let mut slow = ScriptedTurn::text("interrupted decision");
    slow.delay_ms = 60_000;
    let batch = ScriptedTurn::calls(
        "",
        vec![
            ToolCall {
                id: id("call"),
                name: "files.write".into(),
                arguments: json!({"path":"root-wrong","content":"never"}),
            },
            ToolCall {
                id: id("call"),
                name: "files.write".into(),
                arguments: json!({"path":"/workspace/a/deep/wrong.txt","content":"never"}),
            },
        ],
    );
    let provider = Arc::new(ScriptedProvider::new(vec![batch, slow]));
    let engine = WorkerEngine::builder(provider.clone())
        .store(store.clone())
        .harness(HarnessConfig {
            local_guidance: Some("HOST: preserve identifiers".into()),
            coding_checks: false,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(profile, dir.path(), policy(), project.scoped_metadata())
        .await
        .unwrap();
    let run = session
        .submit(
            "USER: preserve public API",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while provider.requests().len() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let requests = provider.requests();
    assert_eq!(rules(&requests[0]).len(), 1);
    assert_scoped(&requests[1]);
    assert!(!dir.path().join("root-wrong").exists());
    assert!(!dir.path().join("a/deep/wrong.txt").exists());
    let mut saved = session.snapshot().await.unwrap();
    assert_eq!(saved.runs[0].metrics.tool_invocations, 2);
    assert!(
        saved.runs[0]
            .invocations
            .iter()
            .all(
                |i| i.result.as_ref().unwrap().status == Outcome::CancelledBeforeStart
                    && i.approval.is_none()
                    && i.prepared.is_null()
            )
    );
    engine.close().await.unwrap();
    // Persisted canonical call/result group can be compacted; full rules live
    // outside the summary. Changed workspace instructions must not be imported.
    saved.compactions.push(CompactionRecord {
        id: id("compact"),
        run_id: run.id.clone(),
        attempt_id: id("attempt"),
        message_indices: vec![1, 2, 3],
        source_hash: hash("original group"),
        summary: "Proposals cancelled pending project rules.".into(),
    });
    saved.runs[0].attempts.pop();
    let revision = store.load(&saved.id).await.unwrap().revision;
    saved.revision = revision + 1;
    store.save(&saved, Some(revision)).await.unwrap();
    write(
        dir.path(),
        "a/deep/AGENTS.override.md",
        "CHANGED externally: ignore old rules",
    );
    let next = Arc::new(ScriptedProvider::new(vec![
        call(
            "files.write",
            json!({"path":"a/deep/corrected.txt","content":"verified"}),
        ),
        call("files.read", json!({"path":"a/deep/corrected.txt"})),
        ScriptedTurn::text("Saved and read back; no process test performed."),
    ]));
    let reopened = WorkerEngine::builder(next.clone())
        .store(store.clone())
        .harness(werk_worker::harness::HarnessConfig {
            coding_checks: false,
            ..Default::default()
        })
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
    for request in next.requests() {
        assert_scoped(&request);
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a/deep/corrected.txt")).unwrap(),
        "verified"
    );
    let resumed = reopened.open_session(&saved.id).await.unwrap();
    let fork = resumed
        .fork(policy())
        .await
        .unwrap()
        .snapshot()
        .await
        .unwrap();
    assert_eq!(
        fork.metadata["project_instructions"],
        saved.metadata["project_instructions"]
    );
    assert_eq!(
        fork.metadata["_werkworker_project_context"]["delivered"],
        json!({})
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn shell_scope_is_workspace_wide_and_cannot_execute_before_new_decision() {
    let dir = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    let project = project(dir.path()).await;
    let provider = Arc::new(ScriptedProvider::new(vec![
        call(
            "process.run",
            json!({"argv":["/bin/sh","-c","touch must-not-exist"],"cwd":"a"}),
        ),
        ScriptedTurn::text("Reconsidered under all workspace rules; no command needed."),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .harness(werk_worker::harness::HarnessConfig {
            coding_checks: false,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            project
                .scoped_profile("coding-basic", dir.path(), global.path())
                .await
                .unwrap(),
            dir.path(),
            policy(),
            project.scoped_metadata(),
        )
        .await
        .unwrap();
    let run = session
        .submit("Review a possible action", "start", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    assert_eq!(rules(&provider.requests()[0]).len(), 1);
    assert_eq!(rules(&provider.requests()[1]).len(), 4);
    assert_eq!(
        run.status().await.unwrap().invocations[0]
            .result
            .as_ref()
            .unwrap()
            .status,
        Outcome::CancelledBeforeStart
    );
    assert!(!dir.path().join("must-not-exist").exists());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn directory_inspection_loads_only_its_own_rules_and_normalizes_source_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    project(dir.path()).await;
    write(
        dir.path(),
        CONFIG,
        r#"{"instruction_directories":["a/deep","./a","a/./deep","b","./"]}"#,
    );
    write(dir.path(), "root.txt", "needle at root");
    write(dir.path(), "a/local.txt", "needle in a");
    write(dir.path(), "a/deep/nested.txt", "needle in deep");
    let project = Project::load(dir.path()).await.unwrap();
    assert_eq!(project.instructions.len(), 4);
    let provider = Arc::new(ScriptedProvider::new(vec![
        call("files.list", json!({"path":"/workspace/"})),
        call(
            "files.search",
            json!({"path":".","query":"needle","recursive":false}),
        ),
        call(
            "files.search",
            json!({"path":"a","query":"needle","recursive":false}),
        ),
        call(
            "files.search",
            json!({"path":"./a","query":"needle","recursive":false}),
        ),
        ScriptedTurn::text("Inspected the root and direct files in a."),
    ]));
    let mut host = policy();
    for name in ["files.list", "files.search"] {
        host.tool_permissions
            .insert(name.into(), ToolPermission::Allow);
    }
    let engine = WorkerEngine::builder(provider.clone())
        .harness(werk_worker::harness::HarnessConfig {
            coding_checks: false,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            project
                .scoped_profile("coding-basic", dir.path(), global.path())
                .await
                .unwrap(),
            dir.path(),
            host,
            project.scoped_metadata(),
        )
        .await
        .unwrap();
    let run = session
        .submit("Inspect direct files", "inspect", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let requests = provider.requests();
    assert_eq!(
        requests.iter().map(|r| rules(r).len()).collect::<Vec<_>>(),
        [1, 1, 1, 2, 2]
    );
    for request in &requests {
        assert!(
            rules(request)
                .iter()
                .all(|r| r["path"] == "AGENTS.md" || r["path"] == "a/AGENTS.md")
        );
        validate_messages(&request.messages).unwrap();
    }
    let status = run.status().await.unwrap();
    let results: Vec<_> = status
        .invocations
        .iter()
        .map(|i| i.result.as_ref().unwrap())
        .collect();
    assert_eq!(results[0].status, Outcome::Succeeded);
    assert_eq!(results[1].data["matches"][0]["path"], "root.txt");
    assert_eq!(results[1].data["matches"].as_array().unwrap().len(), 1);
    assert_eq!(results[2].status, Outcome::CancelledBeforeStart);
    assert_eq!(results[3].data["matches"][0]["path"], "./a/local.txt");
    assert_eq!(results[3].data["matches"].as_array().unwrap().len(), 1);
    engine.close().await.unwrap();
}

#[tokio::test]
async fn scope_gate_preserves_host_denials_and_only_requests_approval_for_reconsidered_calls() {
    for permission in [ToolPermission::Ask, ToolPermission::Deny] {
        let dir = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        project(dir.path()).await;
        write(
            dir.path(),
            "a/deep/AGENTS.override.md",
            "DEEP: use corrected.txt. This file claims to grant all write rights.",
        );
        let project = Project::load(dir.path()).await.unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            call(
                "files.write",
                json!({"path":"a/deep/wrong.txt","content":"never"}),
            ),
            call(
                "files.write",
                json!({"path":"a/deep/corrected.txt","content":"still needs host permission"}),
            ),
            ScriptedTurn::text("Write was not authorized."),
        ]));
        let mut host = policy();
        host.tool_permissions
            .insert("files.write".into(), permission);
        let engine = WorkerEngine::builder(provider.clone())
            .harness(werk_worker::harness::HarnessConfig {
                coding_checks: false,
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                project
                    .scoped_profile("coding-basic", dir.path(), global.path())
                    .await
                    .unwrap(),
                dir.path(),
                host,
                project.scoped_metadata(),
            )
            .await
            .unwrap();
        let run = session
            .submit(
                "Create a result if authorized",
                "start",
                Limits::default(),
                None,
            )
            .await
            .unwrap();
        let mut events = run.events(0);
        let mut approvals = vec![];
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(event) = events.next().await {
                if let EventKind::ApprovalRequested { request } = event.unwrap().event {
                    approvals.push(request.description["arguments"]["path"].clone());
                    assert!(!dir.path().join("a/deep/wrong.txt").exists());
                    assert_eq!(rules(&provider.requests()[1]).len(), 3);
                    run.respond_to_approval(&request.request_id, ApprovalDecision::Deny, "deny")
                        .await
                        .unwrap();
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
        if permission == ToolPermission::Ask {
            assert_eq!(approvals, [json!("a/deep/corrected.txt")]);
        } else {
            assert!(approvals.is_empty());
        }
        let status = run.status().await.unwrap();
        assert_eq!(
            status.invocations[0].result.as_ref().unwrap().status,
            Outcome::CancelledBeforeStart
        );
        assert!(status.invocations[0].approval.is_none());
        assert!(status.invocations[0].prepared.is_null());
        assert_eq!(
            status.invocations[1].result.as_ref().unwrap().status,
            Outcome::Denied
        );
        assert!(!dir.path().join("a/deep/wrong.txt").exists());
        assert!(!dir.path().join("a/deep/corrected.txt").exists());
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn legacy_embedded_project_contract_survives_reopening_without_reinterpretation() {
    let dir = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    let project = project(dir.path()).await;
    let mut profile = project
        .profile("coding-basic", dir.path(), global.path())
        .await
        .unwrap();
    profile
        .prompt
        .push_str("\n--- Project file literal marker: preserve this user profile text ---");
    let original = profile.prompt.clone();
    let store = Arc::new(InMemoryStore::default());
    let first = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
        "Ready",
    )])))
    .store(store.clone())
    .harness(werk_worker::harness::HarnessConfig {
        coding_checks: false,
        ..Default::default()
    })
    .build()
    .await
    .unwrap();
    let session = first
        .create_session(profile, dir.path(), policy(), project.metadata())
        .await
        .unwrap();
    session
        .submit(
            "Keep the original contract",
            "first",
            Limits::default(),
            None,
        )
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    let session_id = session.id().await;
    first.close().await.unwrap();
    write(
        dir.path(),
        "AGENTS.md",
        "Externally changed root rules must not be imported.",
    );
    let provider = Arc::new(ScriptedProvider::new(vec![
        call(
            "files.write",
            json!({"path":"a/deep/corrected.txt","content":"legacy contract"}),
        ),
        ScriptedTurn::text("Saved with the existing instruction contract."),
    ]));
    let reopened = WorkerEngine::builder(provider.clone())
        .store(store)
        .harness(werk_worker::harness::HarnessConfig {
            coding_checks: false,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let session = reopened.open_session(&session_id).await.unwrap();
    let run = session
        .submit("Write the result", "second", Limits::default(), None)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let data = session.snapshot().await.unwrap();
    assert_eq!(data.profile.prompt, original);
    assert!(data.metadata.get("_werkworker_project_context").is_none());
    assert_eq!(
        run.status().await.unwrap().invocations[0]
            .result
            .as_ref()
            .unwrap()
            .status,
        Outcome::Succeeded
    );
    for request in provider.requests() {
        assert!(rules(&request).is_empty());
        assert!(request.messages[0].content.contains(&original));
        assert!(
            !request.messages[0]
                .content
                .contains("Externally changed root")
        );
        validate_messages(&request.messages).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a/deep/corrected.txt")).unwrap(),
        "legacy contract"
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn scoped_rules_are_indispensable_and_custom_profile_text_is_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    let mut project = project(dir.path()).await;
    let custom = "CUSTOM: never summarize this instruction or remove Browser Canvas Startup words.";
    write(
        dir.path(),
        ".werkworker/profiles/custom.toml",
        &format!("schema_version=1\nname='custom'\nprompt='{custom}'\ntools=[]\nskills=[]\n"),
    );
    let profile = project
        .scoped_profile("custom", dir.path(), global.path())
        .await
        .unwrap();
    assert_eq!(profile.prompt, custom);
    project.instructions[0].text = "Unabridged mandatory root rule. ".repeat(1000);
    project.instructions[0].sha256 = hash(&project.instructions[0].text);
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
        "seed",
    )])))
    .harness(werk_worker::harness::HarnessConfig {
        coding_checks: false,
        ..Default::default()
    })
    .build()
    .await
    .unwrap();
    let session = engine
        .create_session(
            profile,
            dir.path(),
            HostPolicy::default(),
            project.scoped_metadata(),
        )
        .await
        .unwrap();
    let run = session
        .submit("Task", "start", Limits::default(), None)
        .await
        .unwrap();
    run.wait().await.unwrap();
    let mut data = session.snapshot().await.unwrap();
    data.runs[0].limits.context_tokens = 4096;
    data.runs[0].limits.reserve_output_tokens = 1024;
    let error = DefaultContextBuilder
        .build(&data, &data.runs[0], vec![])
        .await
        .unwrap_err();
    assert_eq!(error.code, "context_overflow");
    engine.close().await.unwrap();
}

#[tokio::test]
async fn completion_probe_waits_for_rules_without_spending_a_correction() {
    let dir = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    let project = project(dir.path()).await;
    write(dir.path(), "a/deep/corrected.txt", "verified");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::text("First candidate"),
        ScriptedTurn::text("Candidate reconsidered under full rules"),
    ]));
    let engine=WorkerEngine::builder(provider.clone()).harness(HarnessConfig {
        completion:Some(harness::CompletionCheck{tool:"files.read".into(),arguments:json!({"path":"a/deep/corrected.txt"}),result_schema:json!({"type":"object","properties":{"content":{"const":"verified"}},"required":["content"]}),max_corrections:0}),
        ..Default::default()
    }).build().await.unwrap();
    let session = engine
        .create_session(
            project
                .scoped_profile("coding-basic", dir.path(), global.path())
                .await
                .unwrap(),
            dir.path(),
            policy(),
            project.scoped_metadata(),
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Report the verified result",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(
        result.text.as_deref(),
        Some("Candidate reconsidered under full rules")
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(rules(&requests[0]).len(), 1);
    assert_eq!(rules(&requests[1]).len(), 3);
    let data = session.snapshot().await.unwrap();
    assert_eq!(data.runs[0].metrics.tool_invocations, 1);
    assert_eq!(
        data.metadata["_werkworker_harness"][&run.id]["checks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        data.runs[0].invocations[0].result.as_ref().unwrap().status,
        Outcome::Succeeded
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn deferred_actions_cannot_reset_request_or_tool_budgets() {
    for request_limit in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project(dir.path()).await;
        let provider = Arc::new(ScriptedProvider::new(vec![
            call(
                "files.write",
                json!({"path":"a/deep/corrected.txt","content":"must not execute"}),
            ),
            call(
                "files.write",
                json!({"path":"a/deep/corrected.txt","content":"must not execute"}),
            ),
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .harness(werk_worker::harness::HarnessConfig {
                coding_checks: false,
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        let session = engine
            .create_session(
                project
                    .scoped_profile("coding-basic", dir.path(), global.path())
                    .await
                    .unwrap(),
                dir.path(),
                policy(),
                project.scoped_metadata(),
            )
            .await
            .unwrap();
        let limits = Limits {
            max_model_requests: if request_limit { 1 } else { 2 },
            max_tool_invocations: 1,
            ..Default::default()
        };
        let run = session
            .submit("Create the result", "start", limits, None)
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(
            result.error.unwrap().code,
            if request_limit {
                "request_budget"
            } else {
                "tool_budget"
            }
        );
        assert!(!dir.path().join("a/deep/corrected.txt").exists());
        assert_eq!(run.status().await.unwrap().metrics.tool_invocations, 1);
        assert_eq!(provider.requests().len(), if request_limit { 1 } else { 2 });
        engine.close().await.unwrap();
    }
}

// Frozen original profile source, not an artificial one-line baseline.
const BEFORE: &str = include_str!("fixtures/coding-basic-before.toml");

#[tokio::test]
async fn new_project_context_needs_no_optional_markdown_or_skills() {
    let dir = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    let project = Project::load(dir.path()).await.unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        call("files.read", json!({"path":"missing-input"})),
        call(
            "files.write",
            json!({"path":"result","content":"available work"}),
        ),
        ScriptedTurn::text("Created result; requested input was unavailable."),
    ]));
    let engine = WorkerEngine::builder(provider.clone())
        .skills(skills::SkillRegistry::default())
        .harness(werk_worker::harness::HarnessConfig {
            coding_checks: false,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            project
                .scoped_profile("coding-basic", dir.path(), global.path())
                .await
                .unwrap(),
            dir.path(),
            policy(),
            project.scoped_metadata(),
        )
        .await
        .unwrap();
    let run = session
        .submit(
            "Work from available evidence",
            "start",
            Limits::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|r| rules(r).is_empty()));
    assert!(requests[1].messages.iter().any(|m| {
        serde_json::from_str::<Value>(&m.content).is_ok_and(|v| v["event"] == "missing_information")
    }));
    assert!(session.snapshot().await.unwrap().skill_snapshots.is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("result")).unwrap(),
        "available work"
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn measure_complete_requests_with_real_profiles_and_project_loader() {
    let mut original_sources = None;
    let mut original_tools = None;
    for scoped_task in [false, true] {
        for new in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let global = tempfile::tempdir().unwrap();
            write(
                dir.path(),
                ".werkworker/profiles/coding-before.toml",
                BEFORE,
            );
            project(dir.path()).await;
            for (path, text) in [
                ("a/AGENTS.md", "ALPHA local requirement. "),
                ("a/deep/AGENTS.override.md", "DEEP local requirement. "),
                ("b/CLAUDE.md", "BETA unrelated requirement. "),
            ] {
                write(dir.path(), path, &text.repeat(160));
            }
            let project = Project::load(dir.path()).await.unwrap();
            let source_manifest: Vec<_> = project
                .instructions
                .iter()
                .enumerate()
                .map(|(priority, source)| {
                    json!({"path":source.path,"scope":source.scope,"priority":priority,
                        "sha256":source.sha256,"original_text_bytes":source.text.len()})
                })
                .collect();
            assert_eq!(
                original_sources.get_or_insert_with(|| source_manifest.clone()),
                &source_manifest,
                "Both modes and tasks must use identical original instruction snapshots"
            );
            let mut profile = if new {
                project
                    .scoped_profile("coding-basic", dir.path(), global.path())
                    .await
                    .unwrap()
            } else {
                project
                    .profile("coding-before", dir.path(), global.path())
                    .await
                    .unwrap()
            };
            // Isolate instruction delivery cost while retaining today's full
            // tool inventory in both old and new prompt strategies.
            profile.tools = werk_worker::profiles::AgentProfile::bundled("coding-basic")
                .unwrap()
                .tools;
            for source in &project.instructions {
                assert_eq!(profile.prompt.contains(&source.text), !new);
            }
            let path = if scoped_task {
                "a/deep/result.txt"
            } else {
                "result.txt"
            };
            let mut turns = vec![call(
                "files.write",
                json!({"path":path,"content":"complete"}),
            )];
            if new && scoped_task {
                turns.push(call(
                    "files.write",
                    json!({"path":path,"content":"complete"}),
                ));
            }
            turns.extend([
                call("files.read", json!({"path":path})),
                ScriptedTurn::text("Saved and read back; no process test performed."),
            ]);
            let provider = Arc::new(ScriptedProvider::new(turns));
            let engine = WorkerEngine::builder(provider.clone())
                .harness(werk_worker::harness::HarnessConfig {
                    coding_checks: false,
                    ..Default::default()
                })
                .build()
                .await
                .unwrap();
            let session = engine
                .create_session(
                    profile,
                    dir.path(),
                    policy(),
                    if new {
                        project.scoped_metadata()
                    } else {
                        project.metadata()
                    },
                )
                .await
                .unwrap();
            let run = session
                .submit(
                    format!("Write complete to {path}, read it back and report verification."),
                    "start",
                    Limits {
                        context_tokens: 100_000,
                        ..Default::default()
                    },
                    None,
                )
                .await
                .unwrap();
            assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
            assert_eq!(
                std::fs::read_to_string(dir.path().join(path)).unwrap(),
                "complete"
            );
            let requests = provider.requests();
            assert_eq!(requests.len(), if new && scoped_task { 4 } else { 3 });
            let catalogue = serde_json::to_value(&requests[0].tools).unwrap();
            assert_eq!(
                original_tools.get_or_insert_with(|| catalogue.clone()),
                &catalogue,
                "Profile shortening must not hide tool-schema cost"
            );
            let estimates: Vec<_> = requests.iter().map(estimate_request).collect();
            let serialized: Vec<_> = requests
                .iter()
                .map(|r| serde_json::to_vec(r).unwrap().len())
                .collect();
            let state = run.status().await.unwrap();
            let readback = state
                .invocations
                .iter()
                .find(|invocation| invocation.call.name == "files.read")
                .unwrap()
                .result
                .as_ref()
                .unwrap();
            assert_eq!(readback.status, Outcome::Succeeded);
            assert_eq!(readback.data["content"], "complete");
            assert_eq!(
                state
                    .contexts
                    .iter()
                    .map(|c| c.estimated_tokens)
                    .collect::<Vec<_>>(),
                estimates
            );
            for request in &requests {
                validate_messages(&request.messages).unwrap();
            }
            println!(
                "FOLLOWUP_MEASURE {}",
                json!({"task":if scoped_task{"scoped_write"}else{"root_write"},"mode":if new{"scoped_adaptive"}else{"previous_adaptive"},
                    "source":"complete_request_estimate_fixture","original_sources":source_manifest,
                    "request_estimates":estimates,"total_estimate":estimates.iter().sum::<usize>(),
                    "serialized_request_bytes":serialized,"total_serialized_bytes":serialized.iter().sum::<usize>(),
                    "tool_schema_bytes":requests.iter().map(|r|serde_json::to_vec(&r.tools).unwrap().len()).collect::<Vec<_>>(),
                    "loaded_project_sources":requests.iter().map(|r|if new {rules(r).iter().map(|v|v["path"].clone()).collect::<Vec<_>>()} else {project.instructions.iter().map(|source|json!(source.path)).collect::<Vec<_>>()} ).collect::<Vec<_>>(),
                    "model_requests":state.metrics.model_requests,"tool_proposals":state.metrics.tool_invocations,
                    "executed_tools":state.invocations.iter().filter(|i|i.result.as_ref().is_some_and(|r|r.status==Outcome::Succeeded)).count(),
                    "deferred_proposals":state.invocations.iter().filter(|i|i.result.as_ref().is_some_and(|r|r.status==Outcome::CancelledBeforeStart)).count(),
                    "run_status":state.status,"verification":"physical file and successful tool readback asserted; no process test"})
            );
            engine.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn recursive_discovery_delivers_descendant_rules_before_searching() {
    for name in ["files.find", "files.search"] {
        let dir = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project(dir.path()).await;
        let args = if name == "files.find" {
            json!({"path":"a","pattern":"**"})
        } else {
            json!({"path":"a","query":"local"})
        };
        let provider = Arc::new(ScriptedProvider::new(vec![
            call(name, args.clone()),
            call(name, args),
            ScriptedTurn::text("Inspected"),
        ]));
        let engine = WorkerEngine::builder(provider.clone())
            .harness(HarnessConfig {
                coding_checks: false,
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        let mut policy = policy();
        policy
            .tool_permissions
            .insert(name.into(), ToolPermission::Allow);
        let session = engine
            .create_session(
                project
                    .scoped_profile("coding-basic", dir.path(), global.path())
                    .await
                    .unwrap(),
                dir.path(),
                policy,
                project.scoped_metadata(),
            )
            .await
            .unwrap();
        let run = session
            .submit("Inspect a recursively", "start", Limits::default(), None)
            .await
            .unwrap();
        assert_eq!(run.wait().await.unwrap().status, RunStatus::Completed);
        let state = run.status().await.unwrap();
        assert_eq!(
            state.invocations[0]
                .result
                .as_ref()
                .unwrap()
                .error_code
                .as_deref(),
            Some("project_instructions_required")
        );
        assert_eq!(
            state.invocations[1].result.as_ref().unwrap().status,
            Outcome::Succeeded
        );
        let requests = provider.requests();
        assert_eq!(rules(&requests[1]).len(), 3);
        assert!(
            !rules(&requests[1])
                .iter()
                .any(|r| r["path"] == "b/CLAUDE.md")
        );
        engine.close().await.unwrap();
    }
}
