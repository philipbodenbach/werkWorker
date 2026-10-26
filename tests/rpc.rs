use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use werk_worker::{
    policy::HostPolicy,
    providers::{ScriptedProvider, ScriptedTurn},
    transport::{METHODS, RpcHost},
    *,
};

#[tokio::test]
async fn rpc_negotiation_dedupe_malformed_input_and_sdk_loop() {
    let workspace = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
        "RPC result",
    )]));
    let engine = WorkerEngine::builder(provider.clone())
        .build()
        .await
        .unwrap();
    let mut host = RpcHost::new(
        engine.clone(),
        workspace.path().into(),
        HostPolicy::default(),
    );
    let dispatch = |id, method: &str, params| transport::RpcRequest {
        id: json!(id),
        method: method.into(),
        params,
    };
    assert_eq!(
        host.dispatch(&dispatch(0, "session.list", Value::Null))
            .await
            .unwrap_err()
            .code,
        "not_initialized"
    );
    let initialized = host
        .dispatch(&dispatch(1, "initialize", json!({"protocol_version":1})))
        .await
        .unwrap();
    assert_eq!(initialized["methods"], json!(METHODS));
    let created = host
        .dispatch(&dispatch(
            2,
            "session.create",
            json!({"profile":"general-readonly"}),
        ))
        .await
        .unwrap();
    let sid = created["session_id"].as_str().unwrap();
    let params = json!({"session_id":sid,"input":"hello","command_id":"one"});
    let first = host
        .dispatch(&dispatch(3, "run.submit", params.clone()))
        .await
        .unwrap();
    let duplicate = host
        .dispatch(&dispatch(4, "run.submit", params))
        .await
        .unwrap();
    assert_eq!(first["run_id"], duplicate["run_id"]);
    let session = engine.open_session(sid).await.unwrap();
    let run = session
        .run(first["run_id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        run.wait().await.unwrap().text.as_deref(),
        Some("RPC result")
    );
    assert_eq!(provider.requests().len(), 1);
    engine.close().await.unwrap();
}
#[tokio::test]
async fn cli_rpc_stdout_is_json_only_and_bad_message_does_not_exit() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("script.json");
    tokio::fs::write(
        &script,
        serde_json::to_vec(&vec![ScriptedTurn::text("done")]).unwrap(),
    )
    .await
    .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_werk-worker"))
        .arg("--workspace")
        .arg(dir.path())
        .arg("--store")
        .arg(dir.path().join("db.sqlite"))
        .arg("--script")
        .arg(script)
        .arg("rpc")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input
        .write_all(
            b"not JSON\n{\"id\":1,\"method\":\"initialize\",\"params\":{\"protocol_version\":1}}\n",
        )
        .await
        .unwrap();
    let bad: Value = serde_json::from_str(&output.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(bad["error"]["code"], "invalid_request");
    let init: Value = serde_json::from_str(&output.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(init["result"]["protocol_version"], 1);
    input
        .write_all(b"{\"id\":2,\"method\":\"shutdown\"}\n")
        .await
        .unwrap();
    let shutdown: Value =
        serde_json::from_str(&output.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(shutdown["result"]["closed"], true);
    drop(input);
    assert!(child.wait().await.unwrap().success());
}
async fn rpc_write(stdin: &mut tokio::process::ChildStdin, id: u64, method: &str, params: Value) {
    let line = format!("{}\n", json!({"id":id,"method":method,"params":params}));
    stdin.write_all(line.as_bytes()).await.unwrap();
}
async fn rpc_read(
    output: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
) -> Value {
    serde_json::from_str(
        &tokio::time::timeout(std::time::Duration::from_secs(10), output.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .unwrap()
}
fn process(path: &Path, workspace: &Path, script: &Path) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(env!("CARGO_BIN_EXE_werk-worker"));
    c.arg("--workspace")
        .arg(workspace)
        .arg("--store")
        .arg(path)
        .arg("--script")
        .arg(script)
        .arg("rpc")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    c
}

#[tokio::test]
async fn pending_interaction_survives_actual_process_kill_and_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let script = dir.path().join("script.json");
    let path = dir.path().join("db.sqlite");
    tokio::fs::write(
        &script,
        serde_json::to_vec(&vec![ScriptedTurn::calls(
            "",
            vec![ToolCall {
                id: "ask".into(),
                name: "interaction.ask".into(),
                arguments: json!({"question":"Which team?"}),
            }],
        )])
        .unwrap(),
    )
    .await
    .unwrap();
    let mut child = process(&path, workspace.path(), &script).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    rpc_write(&mut input, 1, "initialize", json!({"protocol_version":1})).await;
    rpc_read(&mut output).await;
    rpc_write(&mut input, 2, "session.create", json!({})).await;
    let session = rpc_read(&mut output).await["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    rpc_write(
        &mut input,
        3,
        "run.submit",
        json!({"session_id":session,"input":"ask","command_id":"submit"}),
    )
    .await;
    let run = rpc_read(&mut output).await["result"]["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let request = loop {
        rpc_write(
            &mut input,
            4,
            "run.status",
            json!({"session_id":session,"run_id":run}),
        )
        .await;
        let response = rpc_read(&mut output).await;
        if response["result"]["status"] == "waiting_input" {
            break response["result"]["invocations"][0]["interaction"]["request_id"]
                .as_str()
                .unwrap()
                .to_string();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    drop(input);
    drop(output);
    tokio::fs::write(
        &script,
        serde_json::to_vec(&vec![ScriptedTurn::text("Recovered answer")]).unwrap(),
    )
    .await
    .unwrap();
    let mut child = process(&path, workspace.path(), &script).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    rpc_write(&mut input, 1, "initialize", json!({"protocol_version":1})).await;
    rpc_read(&mut output).await;
    rpc_write(&mut input,2,"interaction.respond",json!({"session_id":session,"run_id":run,"request_id":request,"answer":"Operations","command_id":"answer"})).await;
    assert!(rpc_read(&mut output).await.get("result").is_some());
    rpc_write(
        &mut input,
        3,
        "run.resume",
        json!({"session_id":session,"run_id":run}),
    )
    .await;
    assert!(rpc_read(&mut output).await.get("result").is_some());
    loop {
        rpc_write(
            &mut input,
            4,
            "run.status",
            json!({"session_id":session,"run_id":run}),
        )
        .await;
        let response = rpc_read(&mut output).await;
        if response["result"]["status"] == "completed" {
            assert_eq!(response["result"]["final_text"], "Recovered answer");
            break;
        }
        assert_ne!(response["result"]["status"], "failed");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    rpc_write(&mut input, 5, "shutdown", Value::Null).await;
    rpc_read(&mut output).await;
    assert!(child.wait().await.unwrap().success());
}

#[tokio::test]
async fn rpc_subagents_are_scoped_and_can_be_listed_steered_and_cancelled() {
    let workspace = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(
        (0..6)
            .map(|_| ScriptedTurn {
                delay_ms: 3000,
                ..ScriptedTurn::text("done")
            })
            .collect(),
    ));
    let engine = WorkerEngine::builder(provider).build().await.unwrap();
    let mut host = RpcHost::new(
        engine.clone(),
        workspace.path().into(),
        HostPolicy::default(),
    );
    let req = |method: &str, params| transport::RpcRequest {
        id: json!(1),
        method: method.into(),
        params,
    };
    host.dispatch(&req("initialize", json!({"protocol_version":1})))
        .await
        .unwrap();
    assert_eq!(
        host.dispatch(&req("execution.status", json!({})))
            .await
            .unwrap()["effective_model_requests"],
        4
    );
    let sid = host
        .dispatch(&req(
            "session.create",
            json!({"profile":"general-readonly"}),
        ))
        .await
        .unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let run_id = host
        .dispatch(&req(
            "run.submit",
            json!({"session_id":sid,"input":"go","command_id":"root"}),
        ))
        .await
        .unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let tasks = json!({"session_id":sid,"run_id":run_id,"command_id":"children","tasks":[{"goal":"alpha"},{"goal":"beta"}]});
    let children = host
        .dispatch(&req("agents.delegate", tasks.clone()))
        .await
        .unwrap();
    let duplicate = host.dispatch(&req("agents.delegate", tasks)).await.unwrap();
    assert_eq!(children[0]["session_id"], duplicate[0]["session_id"]);
    assert_eq!(
        host.dispatch(&req(
            "agents.list",
            json!({"session_id":sid,"run_id":run_id})
        ))
        .await
        .unwrap()
        .as_array()
        .unwrap()
        .len(),
        2
    );
    host.dispatch(&req("agents.message",json!({"session_id":sid,"run_id":run_id,"child_session_id":children[0]["session_id"],"input":"focus","command_id":"steer"}))).await.unwrap();
    assert_eq!(host.dispatch(&req("agents.cancel",json!({"session_id":sid,"run_id":run_id,"child_session_id":"foreign","command_id":"deny"}))).await.unwrap_err().code,"agent_denied");
    host.dispatch(&req("agents.cancel",json!({"session_id":sid,"run_id":run_id,"child_session_id":children[0]["session_id"],"command_id":"cancel-child"}))).await.unwrap();
    host.dispatch(&req(
        "run.cancel",
        json!({"session_id":sid,"run_id":run_id,"command_id":"cancel-root"}),
    ))
    .await
    .unwrap();
    let run = engine
        .open_session(&sid)
        .await
        .unwrap()
        .run(&run_id)
        .await
        .unwrap();
    assert_eq!(run.wait().await.unwrap().status, RunStatus::Cancelled);
    assert!(
        run.subagents()
            .await
            .unwrap()
            .iter()
            .all(|c| c.run.as_ref().unwrap().status.terminal())
    );
    engine.close().await.unwrap();
}
