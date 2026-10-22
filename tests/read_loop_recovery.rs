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
fn read(path: &str) -> ScriptedTurn {
    call("files.read", json!({"path":path}))
}
fn edit(before: &str, after: &str) -> ScriptedTurn {
    call(
        "files.edit",
        json!({"path":"game.txt","expected_hash":hash(before),"old":before,"new":after}),
    )
}
async fn fixture(
    turns: Vec<ScriptedTurn>,
    dir: &std::path::Path,
    denied: bool,
) -> (WorkerEngine, SessionHandle, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider::new(turns));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let names = [
        "files.read",
        "files.list",
        "files.write",
        "files.edit",
        "process.run",
    ];
    let session = engine
        .create_session(
            AgentProfile {
                schema_version: 1,
                name: "coding-basic".into(),
                prompt: "Implement and verify".into(),
                tools: names.map(str::to_owned).to_vec(),
                skills: vec![],
            },
            dir,
            HostPolicy {
                workspace_writable: true,
                tool_permissions: names.map(|n| (n.into(), ToolPermission::Allow)).into(),
                hard_denied_tools: if denied {
                    vec!["files.read".into()]
                } else {
                    vec![]
                },
                ..Default::default()
            },
            Value::Null,
        )
        .await
        .unwrap();
    (engine, session, provider)
}

#[tokio::test]
async fn blocked_reads_recover_to_incremental_edits_and_verified_completion() {
    if let Err(error) = runtime::Sandbox::default().doctor().await {
        assert!(std::env::var_os("REQUIRE_SANDBOX").is_none(), "{error}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (engine,session,provider) = fixture(vec![
        call("files.write",json!({"path":"game.txt","content":"one"})),
        read("game.txt"), read("game.txt"),
        call("files.write",json!({"path":"game.txt","content":"overwrite"})),
        edit("one","two"), read("game.txt"), read("game.txt"), edit("two","three"),
        call("process.run",json!({"argv":["/bin/sh","-c","test \"$(cat game.txt)\" = three"],"verification":{"criteria":["Final fixture content is three"],"paths":["game.txt"]}})),
        ScriptedTurn::text("Verified final content"),
    ],dir.path(),false).await;
    let run = session
        .submit(
            "Implement",
            "start",
            Limits {
                max_repeated_calls: 1,
                ..Limits::continuous()
            },
            None,
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(result.status, RunStatus::Completed, "{:?}", result.error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("game.txt")).unwrap(),
        "three"
    );
    let data = session.snapshot().await.unwrap();
    let invocations = &data.run(&run.id).unwrap().invocations;
    let recoveries: Vec<_> = invocations
        .iter()
        .filter_map(|i| i.result.as_ref())
        .filter(|r| r.error_code.as_deref() == Some("repetition_recovery"))
        .collect();
    assert_eq!(recoveries.len(), 2);
    assert!(
        recoveries
            .iter()
            .all(|r| r.data["executed"] == false && r.data["recovery"] == 1)
    );
    let conflict = invocations[3].result.as_ref().unwrap();
    assert_eq!(conflict.error_code.as_deref(), Some("write_conflict"));
    assert!(
        conflict.data["message"]
            .as_str()
            .unwrap()
            .contains("files.edit")
    );
    assert!(
        !conflict.data["message"]
            .as_str()
            .unwrap()
            .contains("supply expected_hash")
    );
    assert!(invocations[8].result.as_ref().unwrap().data["verification"]["passed"] == true);
    for request in provider.requests() {
        validate_messages(&request.messages).unwrap();
    }
    engine.close().await.unwrap();
}

#[tokio::test]
async fn alternating_reads_and_failed_writes_cannot_replenish_recovery_or_bypass_denials() {
    for denied in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "a").unwrap();
        std::fs::write(dir.path().join("b"), "b").unwrap();
        let turns = if denied {
            vec![read("a"), read("a")]
        } else {
            vec![
                read("a"),
                read("b"),
                read("a"),
                read("a"),
                call(
                    "files.write",
                    json!({"path":"a","content":"failed overwrite"}),
                ),
                read("b"),
                read("a"),
            ]
        };
        let (engine, session, _) = fixture(turns, dir.path(), denied).await;
        let run = session
            .submit(
                "Inspect and implement",
                "start",
                Limits {
                    max_repeated_calls: 1,
                    ..Limits::continuous()
                },
                None,
            )
            .await
            .unwrap();
        let result = run.wait().await.unwrap();
        assert_eq!(result.error.unwrap().code, "repetition_limit");
        let saved: SessionData = serde_json::from_slice(
            &serde_json::to_vec(&session.snapshot().await.unwrap()).unwrap(),
        )
        .unwrap();
        let recoveries: Vec<_> = saved
            .run(&run.id)
            .unwrap()
            .invocations
            .iter()
            .filter_map(|i| i.result.as_ref())
            .filter(|r| r.error_code.as_deref() == Some("repetition_recovery"))
            .collect();
        assert_eq!(recoveries.len(), if denied { 0 } else { 2 });
        if !denied {
            assert_eq!(recoveries[1].data["recovery"], 2);
        }
        assert_eq!(std::fs::read_to_string(dir.path().join("a")).unwrap(), "a");
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn compacted_coding_context_ends_with_observed_files_instead_of_stale_empty_listing() {
    let dir = tempfile::tempdir().unwrap();
    let html = format!("{}\n</html>", "<!-- fixture -->\n".repeat(170));
    let js = format!(
        "{}\n// NEXT_SECTION",
        "// existing implementation\n".repeat(120)
    );
    let (engine, session, _) = fixture(
        vec![
            call("files.list", json!({"path":"."})),
            call("files.write", json!({"path":"index.html","content":html})),
            call("files.write", json!({"path":"game.js","content":js})),
            read("index.html"),
            read("game.js"),
        ],
        dir.path(),
        false,
    )
    .await;
    let run = session
        .submit(
            "Keep the existing interface",
            "start",
            Limits {
                max_steps: 5,
                ..Limits::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().error.unwrap().code, "step_budget");
    let mut data = session.snapshot().await.unwrap();
    let index = data.run(&run.id).unwrap().invocations[0].message_index;
    data.run_mut(&run.id).unwrap().limits.context_tokens = 14000;
    data.run_mut(&run.id).unwrap().limits.reserve_output_tokens = 4096;
    let (request, snapshot) = DefaultContextBuilder
        .build(&data, data.run(&run.id).unwrap(), vec![])
        .await
        .unwrap();
    validate_messages(&request.messages).unwrap();
    assert!(
        snapshot.omitted_messages.contains(&index),
        "old empty listing must be compactable: {:?}",
        snapshot.omitted_messages
    );
    let position = request
        .messages
        .iter()
        .rposition(|m| m.content.contains("\"event\":\"coding_task_state\""))
        .unwrap();
    assert!(
        request.messages[position + 1..]
            .iter()
            .all(|m| m.role != Role::Tool)
    );
    assert_eq!(request.messages[position].role, Role::Assistant);
    let evidence: Value = serde_json::from_str(&request.messages[position].content).unwrap();
    let files = evidence["data"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    let game = files.iter().find(|f| f["path"] == "game.js").unwrap();
    assert_eq!(game["sha256"], hash(&js));
    assert!(
        game["observed_tail_excerpt"]
            .as_str()
            .unwrap()
            .ends_with("// NEXT_SECTION")
    );
    assert!(request.messages[0].content.find("NEXT_SECTION").is_none());
    engine.close().await.unwrap();
}
