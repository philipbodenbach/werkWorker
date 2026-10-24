use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{
    context::{ContextBuilder, DefaultContextBuilder, validate_messages},
    policy::*,
    profiles::AgentProfile,
    providers::*,
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

fn plan(status: &str) -> ScriptedTurn {
    call(
        "task.plan",
        json!({
            "steps":[{"id":"version","description":"Runnable counter with a verified increment","status":status}],
            "checkpoint":{"next_action":"Connect increment to the exported count, then assert increment(0) == 1",
                "decisions":["counter.js exports increment(n); use the public function in the check"]}
        }),
    )
}

fn verify() -> ScriptedTurn {
    call(
        "process.run",
        json!({
            "argv":["node","-e","const {increment}=require('./counter.js');require('assert').equal(increment(0),1)"],
            "verification":{"criteria":["Public increment advances the count"],"paths":["counter.js"]}
        }),
    )
}

fn cycle(request: &ModelRequest) -> Option<Value> {
    request
        .messages
        .iter()
        .filter_map(|m| serde_json::from_str::<Value>(&m.content).ok())
        .find(|v| v["event"] == "coding_task_state")
        .map(|v| v["data"]["work_cycle"].clone())
}

async fn fixture(
    turns: Vec<ScriptedTurn>,
    path: &std::path::Path,
) -> (WorkerEngine, SessionHandle, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider::new(turns));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "coding-basic".into(),
                prompt: "Implement and verify the requested behavior".into(),
                tools: [
                    "files.write",
                    "files.edit",
                    "files.read",
                    "files.search",
                    "task.plan",
                    "process.run",
                ]
                .map(str::to_owned)
                .to_vec(),
                skills: vec![],
            },
            path,
            HostPolicy {
                workspace_writable: true,
                tool_permissions: [
                    "files.write",
                    "files.edit",
                    "files.read",
                    "files.search",
                    "process.run",
                ]
                .map(|n| (n.into(), ToolPermission::Allow))
                .into(),
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    (engine, session, provider)
}

#[tokio::test]
async fn wandering_reads_are_redirected_then_real_review_and_repair_finish() {
    if let Err(error) = runtime::Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let initial = "exports.increment = n => n;\n";
    let repaired = "exports.increment = n => n + 1;\n";
    let mut turns = vec![
        plan("in_progress"),
        call(
            "files.write",
            json!({"path":"counter.js","content":initial}),
        ),
    ];
    // Successful, differently spelled shell reads bypass exact-call and failed-
    // command guards. A plan update also isn't implementation progress.
    for n in 0..5 {
        turns.push(call(
            "process.run",
            json!({"argv":["/bin/sh","-c",format!("cat counter.js # inspection {n}")]}),
        ));
    }
    turns.extend([
        plan("in_progress"),
        verify(),
        call(
            "files.edit",
            json!({"path":"counter.js","expected_hash":hash(initial),"old":initial,"new":repaired}),
        ),
        verify(),
        plan("completed"),
        ScriptedTurn::text("Implemented and verified increment"),
    ]);
    let (engine, session, provider) = fixture(turns, dir.path()).await;
    let result = session
        .submit(
            "Build a counter whose public increment increases it by one",
            "start",
            Limits::continuous(),
            None,
        )
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("counter.js")).unwrap(),
        repaired
    );
    let requests = provider.requests();
    let redirected = &requests[8];
    assert_eq!(cycle(redirected).unwrap()["refocus"], true);
    let steering = redirected.messages.last().unwrap();
    assert_eq!(steering.role, Role::User);
    assert!(steering.content.contains("Runtime work-cycle recovery"));
    assert!(steering.content.contains("smallest runnable end-to-end"));
    let failed = cycle(&requests[9]).unwrap();
    assert_eq!(failed["suggested_phase"], "repair");
    assert!(
        failed["latest_check"]["failure_excerpt"]
            .as_str()
            .unwrap()
            .contains("AssertionError")
    );
    assert_eq!(cycle(&requests[10]).unwrap()["suggested_phase"], "build");
    assert_eq!(cycle(&requests[11]).unwrap()["suggested_phase"], "review");
    assert_eq!(cycle(&requests[11]).unwrap()["refocus"], false);
    assert!(
        requests[11]
            .messages
            .iter()
            .all(|m| !m.content.contains("Runtime work-cycle recovery"))
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn checkpoint_and_goal_survive_missing_transcript_and_new_user_steering() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, session, _) = fixture(
        vec![
            plan("in_progress"),
            call(
                "files.write",
                json!({"path":"counter.js","content":"exports.increment = n => n;\n"}),
            ),
        ],
        dir.path(),
    )
    .await;
    session
        .submit(
            "Build a public counter API",
            "start",
            Limits {
                max_steps: 2,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    let saved = session.snapshot().await.unwrap();
    let mut restored: SessionData =
        serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
    // Simulate removal of every tool transcript. Durable invocation receipts
    // and the task plan still exist, as with rolling compaction/resume.
    restored.messages = vec![
        Message::text(Role::User, "Build a public counter API"),
        Message::text(Role::User, "Also handle negative input"),
    ];
    let run = restored.runs.last_mut().unwrap();
    run.history_start = 0;
    run.contexts.clear();
    run.steering.push("Also handle negative input".into());
    let (request, _) = DefaultContextBuilder
        .build(&restored, restored.runs.last().unwrap(), vec![])
        .await
        .unwrap();
    validate_messages(&request.messages).unwrap();
    let progress = cycle(&request).unwrap();
    assert_eq!(progress["objective_excerpt"], "Build a public counter API");
    assert_eq!(
        progress["latest_user_update_excerpt"],
        "Also handle negative input"
    );
    assert_eq!(
        progress["current_milestone"],
        "Runnable counter with a verified increment"
    );
    assert_eq!(progress["last_change"]["path"], "counter.js");
    let evidence = request
        .messages
        .iter()
        .find(|m| m.content.contains("coding_task_state"))
        .unwrap();
    assert!(
        evidence
            .content
            .contains("Connect increment to the exported count")
    );
    assert!(evidence.content.contains("counter.js exports increment(n)"));
    assert_eq!(evidence.role, Role::Assistant); // Checkpoint data gains no system authority.
    engine.close().await.unwrap();
}

#[tokio::test]
async fn investigation_before_any_implementation_does_not_force_edits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("input.txt"), "a\nb\nc\nd\ne\nf\ng\nh\n").unwrap();
    let mut turns = vec![plan("in_progress")];
    turns.extend((1..=8).map(|n| {
        call(
            "files.read",
            json!({"path":"input.txt","start_line":n,"end_line":n}),
        )
    }));
    let (engine, session, provider) = fixture(turns, dir.path()).await;
    session
        .submit(
            "Investigate this data only",
            "start",
            Limits {
                max_steps: 9,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(
        provider
            .requests()
            .iter()
            .filter_map(cycle)
            .all(|v| v["refocus"] == false)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("input.txt")).unwrap(),
        "a\nb\nc\nd\ne\nf\ng\nh\n"
    );
    engine.close().await.unwrap();
}
