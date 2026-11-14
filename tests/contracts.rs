use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{
    policy::HostPolicy,
    profiles::AgentProfile,
    providers::{ScriptedProvider, ScriptedTurn},
    sessions::SqliteStore,
    skills::SkillRegistry,
    tools::ToolRegistry,
    transport::METHODS,
    *,
};

#[test]
fn invalid_tool_arguments_explain_constraints_without_echoing_secrets() {
    use werk_worker::tools::validate_arguments;
    let schema = json!({
        "type":"object", "properties":{"source":{"type":"string"},"destination":{"type":"string"}},
        "additionalProperties":false,
        "oneOf":[{"required":["source"]},{"required":["destination"]}]
    });
    for args in [
        json!({}),
        json!({"source":"private-token", "destination":"private-token"}),
    ] {
        let error = validate_arguments(&schema, &args).unwrap_err();
        assert!(error.message.contains("exactly one alternative"));
        assert!(error.message.contains("source"));
        assert!(error.message.contains("destination"));
        assert!(!error.message.contains("private-token"));
    }
    let error = validate_arguments(&json!({"type":"object","required":["target"]}), &json!({}))
        .unwrap_err();
    assert!(error.message.contains("required fields: [\"target\"]"));
    let error =
        validate_arguments(&json!({"type":"integer"}), &json!("private-token")).unwrap_err();
    assert!(error.message.contains("expected type: \"integer\""));
    assert!(!error.message.contains("private-token"));
}

#[test]
fn bundled_profile_tool_skill_references_and_cli_help_are_consistent() {
    let tools = ToolRegistry::builtins().unwrap();
    let skills = SkillRegistry::bundled().unwrap();
    for name in AgentProfile::names() {
        let profile = AgentProfile::bundled(name).unwrap();
        for tool in profile.tools {
            tools.get(&tool).unwrap();
        }
        for skill in profile.skills {
            skills.get(&skill).unwrap();
        }
    }
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_werk-worker"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let short_help = std::process::Command::new(env!("CARGO_BIN_EXE_ww"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(short_help.status.success());
    assert!(
        String::from_utf8(short_help.stdout)
            .unwrap()
            .contains("Usage: ww")
    );
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(!text.contains("--harness <"));
    assert!(text.contains("--harness-config"));
    assert!(!text.contains("classic"));
    assert!(!text.contains("minimal"));
    for command in [
        "tui", "run", "resume", "sessions", "tools", "skills", "profiles", "sandbox", "rpc",
    ] {
        assert!(text.contains(command));
    }
    let schema: Value = serde_json::from_str(include_str!("../docs/protocol.schema.json")).unwrap();
    let methods = schema["properties"]["method"]["enum"].as_array().unwrap();
    assert_eq!(
        methods,
        &METHODS.iter().map(|m| json!(m)).collect::<Vec<_>>()
    );
    assert_eq!(
        include_str!("../LICENSE").lines().next(),
        Some("Elastic License 2.0")
    );
    assert!(include_str!("../Cargo.toml").contains("license = \"Elastic-2.0\""));
}
#[tokio::test]
async fn control_store_cannot_be_mounted_as_agent_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteStore::open(dir.path().join("worker.sqlite"))
            .await
            .unwrap(),
    );
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
        "unused",
    )])))
    .store(store)
    .build()
    .await
    .unwrap();
    assert_eq!(
        engine
            .create_session(
                AgentProfile::bundled("coding-basic").unwrap(),
                dir.path(),
                HostPolicy::default(),
                Value::Null
            )
            .await
            .err()
            .unwrap()
            .code,
        "unsafe_workspace"
    );
    engine.close().await.unwrap();
}
