#![cfg(target_os = "linux")]
use std::path::Path;
use werk_worker::{
    policy::HostPolicy,
    profiles::AgentProfile,
    project::*,
    providers::{ScriptedProvider, ScriptedTurn},
    sessions::InMemoryStore,
    *,
};

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn delegation_level_is_validated_saved_and_inherited_without_changing_other_defaults() {
    use werk_worker::parallel::DelegationLevel;
    assert!(
        ProjectConfig::parse(b"{}")
            .unwrap()
            .delegation_level
            .is_none()
    );
    assert_eq!(
        DelegationLevel::from_metadata(&serde_json::Value::Null).unwrap(),
        DelegationLevel::Active
    );
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        CONFIG,
        r#"{"model":"parent","subagent_model":"helper","delegation_level":"active"}"#,
    );
    for level in DelegationLevel::ALL {
        let project = Project::load(dir.path()).await.unwrap();
        let mut config = project.config.clone();
        config.delegation_level = Some(level);
        project.save(dir.path(), &config).await.unwrap();
        let restored = Project::load(dir.path()).await.unwrap();
        assert_eq!(restored.config.delegation_level, Some(level));
        assert_eq!(restored.config.subagent_model.as_deref(), Some("helper"));
        assert_eq!(
            DelegationLevel::from_metadata(&restored.scoped_metadata()).unwrap(),
            level
        );
    }
    for invalid in [
        serde_json::json!("aggressive"),
        serde_json::json!(4),
        serde_json::json!(true),
    ] {
        assert!(
            ProjectConfig::parse(
                &serde_json::to_vec(&serde_json::json!({"delegation_level":invalid})).unwrap()
            )
            .is_err()
        );
    }
}

#[tokio::test]
async fn optional_subagent_model_survives_project_settings_save() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        CONFIG,
        r#"{"version":1,"model":"main","subagent_model":"helper"}"#,
    );
    let project = Project::load(dir.path()).await.unwrap();
    let mut config = project.config.clone();
    config.profile = Some("coding-basic".into());
    project.save(dir.path(), &config).await.unwrap();
    assert_eq!(
        Project::load(dir.path())
            .await
            .unwrap()
            .config
            .subagent_model
            .as_deref(),
        Some("helper")
    );
    for bytes in [b"{}".as_slice(), br#"{"subagent_model":null}"#] {
        let config = ProjectConfig::parse(bytes).unwrap();
        assert!(config.subagent_model.is_none());
        assert!(
            serde_json::to_value(config)
                .unwrap()
                .get("subagent_model")
                .is_none()
        );
    }
    for model in [
        "".to_owned(),
        "  ".into(),
        "bad\nmodel".into(),
        "x".repeat(513),
    ] {
        assert!(
            ProjectConfig::parse(
                &serde_json::to_vec(&serde_json::json!({"subagent_model":model})).unwrap()
            )
            .is_err()
        );
    }
}

#[test]
fn project_reasoning_distinguishes_inheritance_from_explicit_server_default() {
    let inherited = ProjectConfig::parse(b"{}").unwrap();
    assert!(inherited.reasoning_effort.is_none());
    assert!(
        serde_json::to_value(&inherited)
            .unwrap()
            .get("reasoning_effort")
            .is_none()
    );
    for value in [
        serde_json::Value::Null,
        serde_json::json!("none"),
        serde_json::json!("high"),
        serde_json::json!(0.75),
    ] {
        let bytes = serde_json::to_vec(&serde_json::json!({"reasoning_effort":value})).unwrap();
        let config = ProjectConfig::parse(&bytes).unwrap();
        assert_eq!(config.reasoning_effort, Some(value.clone()));
        let roundtrip = ProjectConfig::parse(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(roundtrip.reasoning_effort, Some(value));
    }
    for value in [
        serde_json::json!(true),
        serde_json::json!([]),
        serde_json::json!({}),
    ] {
        assert_eq!(
            ProjectConfig::parse(
                &serde_json::to_vec(&serde_json::json!({"reasoning_effort":value})).unwrap()
            )
            .unwrap_err()
            .code,
            "invalid_reasoning_effort"
        );
    }
}
#[tokio::test]
async fn connection_defaults_stay_within_workspace_and_reject_unsafe_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let child = dir.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let config = werk_worker::providers::WerkConfig::local("fixture-model");
    write(
        dir.path(),
        CONNECTION_CONFIG,
        &serde_json::to_string(&config).unwrap(),
    );
    assert!(Project::connection_config(&child).await.unwrap().is_none());
    assert_eq!(
        Project::connection_config(dir.path())
            .await
            .unwrap()
            .unwrap()
            .werk
            .model,
        "fixture-model"
    );
    write(
        dir.path(),
        CONNECTION_SECRETS,
        r#"{"api_key":"fixture-key","unknown":true}"#,
    );
    let error = Project::connection_config(dir.path()).await.err().unwrap();
    assert_eq!(error.code, "project_credentials");
    assert!(!error.to_string().contains("fixture-key"));
    std::fs::remove_file(dir.path().join(CONNECTION_SECRETS)).unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(
        outside.path(),
        "secret.json",
        r#"{"api_key":"outside-key"}"#,
    );
    std::os::unix::fs::symlink(
        outside.path().join("secret.json"),
        dir.path().join(CONNECTION_SECRETS),
    )
    .unwrap();
    assert_eq!(
        Project::connection_config(dir.path())
            .await
            .err()
            .unwrap()
            .code,
        "project_path"
    );
    std::fs::remove_file(dir.path().join(CONNECTION_SECRETS)).unwrap();
    write(dir.path(), CONNECTION_CONFIG, &"x".repeat(65_537));
    assert_eq!(
        Project::connection_config(dir.path())
            .await
            .err()
            .unwrap()
            .code,
        "file_limit"
    );
}

#[tokio::test]
async fn priority_scope_and_immutable_session_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    write(dir.path(), "AGENTS.md", "ignored agents marker");
    write(dir.path(), "CLAUDE.md", "ignored claude marker");
    write(dir.path(), "AGENTS.override.md", "personal root marker");
    write(dir.path(), "src/AGENTS.md", "source marker");
    write(dir.path(), "src/deep/AGENTS.md", "deep marker");
    write(
        dir.path(),
        CONFIG,
        r#"{"version":1,"instruction_directories":["src/deep"],"test_commands":{"unit":["cargo","test"]},"requested_access":"process"}"#,
    );
    let project = Project::load(dir.path()).await.unwrap();
    assert_eq!(
        project
            .instructions
            .iter()
            .map(|r| r.path.as_str())
            .collect::<Vec<_>>(),
        ["AGENTS.override.md", "src/AGENTS.md", "src/deep/AGENTS.md"]
    );
    let profile = project
        .profile("general-readonly", dir.path(), global.path())
        .await
        .unwrap();
    assert!(profile.prompt.contains("personal root marker"));
    assert!(!profile.prompt.contains("ignored agents marker"));
    assert!(!profile.prompt.contains("ignored claude marker"));
    assert!(profile.prompt.contains("Suggested test commands"));
    write(dir.path(), "AGENTS.override.md", "changed after snapshot");
    assert!(profile.prompt.contains("personal root marker"));
    let engine = WorkerEngine::builder(std::sync::Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::text("done"),
    ])))
    .store(std::sync::Arc::new(InMemoryStore::default()))
    .build()
    .await
    .unwrap();
    let session = engine
        .create_session(
            profile,
            dir.path(),
            HostPolicy::default(),
            project.metadata(),
        )
        .await
        .unwrap();
    let data = session.snapshot().await.unwrap();
    assert!(data.policy.grants.is_empty());
    assert!(!data.policy.workspace_writable);
    assert_eq!(
        data.metadata["project_instructions"][0]["sha256"],
        hash("personal root marker")
    );
    engine.close().await.unwrap();
    write(dir.path(), "WERKWORKER.md", "werk uppercase marker");
    write(dir.path(), ".werkworker.md", "werk dot marker");
    assert_eq!(
        Project::load(dir.path()).await.unwrap().instructions[0].text,
        "werk dot marker"
    );
}
#[tokio::test]
async fn custom_profiles_tools_skills_and_explicit_access_are_separate() {
    let dir = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    let custom = AgentProfile {
        schema_version: 1,
        name: "reviewer".into(),
        prompt: "global marker".into(),
        tools: vec!["files.read".into()],
        skills: vec![],
    };
    write(
        global.path(),
        "profiles/reviewer.toml",
        &toml::to_string(&custom).unwrap(),
    );
    write(
        dir.path(),
        CONFIG,
        r#"{"profile":"reviewer","tools":["files.read","process.run"],"skills":["coding-basic"],"requested_access":"process"}"#,
    );
    let project = Project::load(dir.path()).await.unwrap();
    let profile = project
        .profile("reviewer", dir.path(), global.path())
        .await
        .unwrap();
    assert_eq!(profile.prompt, "global marker");
    assert_eq!(profile.tools, ["files.read", "process.run"]);
    assert_eq!(profile.skills, ["coding-basic"]);
    assert!(
        Project::profile_names(dir.path(), global.path())
            .await
            .unwrap()
            .contains(&"reviewer".into())
    );
    let mut local = custom;
    local.prompt = "local marker".into();
    write(
        dir.path(),
        ".werkworker/profiles/reviewer.toml",
        &toml::to_string(&local).unwrap(),
    );
    assert_eq!(
        project
            .profile("reviewer", dir.path(), global.path())
            .await
            .unwrap()
            .prompt,
        "local marker"
    );
    let read = Access::Read.policy();
    assert!(!read.workspace_writable);
    assert!(
        read.grants
            .iter()
            .all(|g| g.tool.starts_with("files.") && !g.preapproved)
    );
    let process = Access::Process.policy();
    assert!(process.workspace_writable);
    assert!(process.grants.iter().all(|g| !g.preapproved));
    let mut optional = project.clone();
    optional.config.skills = Some(vec!["unknown".into()]);
    assert_eq!(
        optional
            .profile("reviewer", dir.path(), global.path())
            .await
            .unwrap()
            .skills,
        ["unknown"]
    );
}
#[tokio::test]
async fn save_detects_conflicts_and_never_grants_authority() {
    let dir = tempfile::tempdir().unwrap();
    let project = Project::load(dir.path()).await.unwrap();
    let config = ProjectConfig {
        model: Some("model-a".into()),
        requested_access: Some(Access::Process),
        ..Default::default()
    };
    project.save(dir.path(), &config).await.unwrap();
    assert_eq!(
        Project::load(dir.path())
            .await
            .unwrap()
            .config
            .model
            .as_deref(),
        Some("model-a")
    );
    assert_eq!(
        project.save(dir.path(), &config).await.unwrap_err().code,
        "write_conflict"
    );
    let loaded = Project::load(dir.path()).await.unwrap();
    write(dir.path(), CONFIG, "{}");
    assert_eq!(
        loaded.save(dir.path(), &config).await.unwrap_err().code,
        "write_conflict"
    );
}
#[tokio::test]
async fn rejects_symlinks_oversize_traversal_and_host_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(outside.path(), "AGENTS.md", "outside secret marker");
    std::os::unix::fs::symlink(
        outside.path().join("AGENTS.md"),
        dir.path().join("AGENTS.md"),
    )
    .unwrap();
    assert_eq!(
        Project::load(dir.path()).await.unwrap_err().code,
        "project_path"
    );
    for json in [
        r#"{"base_url":"https://untrusted.example"}"#,
        r#"{"secret_env":"SECRET"}"#,
        r#"{"grants":[]}"#,
        r#"{"sandbox":{"network":true}}"#,
        r#"{"instruction_directories":["../outside"]}"#,
        r#"{"profile":"../outside"}"#,
        r#"{"version":2}"#,
    ] {
        assert!(ProjectConfig::parse(json.as_bytes()).is_err(), "{json}");
    }
    let huge = tempfile::tempdir().unwrap();
    write(huge.path(), "AGENTS.md", &"x".repeat(16_385));
    assert_eq!(
        Project::load(huge.path()).await.unwrap_err().code,
        "file_limit"
    );
}
#[test]
fn protocol_reasoning_tier_project_configuration_is_explicit_and_validated() {
    use werk_worker::project::ProjectConfig;
    for value in [
        serde_json::json!("ultra"),
        serde_json::json!("default"),
        serde_json::Value::Null,
    ] {
        let config = ProjectConfig::parse(
            &serde_json::to_vec(&serde_json::json!({"reasoning_tier":value})).unwrap(),
        )
        .unwrap();
        assert_eq!(config.reasoning_tier, Some(value));
        assert!(
            serde_json::to_value(config)
                .unwrap()
                .get("reasoning_tier")
                .is_some()
        );
    }
    for payload in [
        serde_json::json!({"reasoning_tier":42}),
        serde_json::json!({"reasoning_tier":"bad tier"}),
        serde_json::json!({"reasoning_tier":"high","reasoning_effort":"low"}),
    ] {
        assert!(ProjectConfig::parse(&serde_json::to_vec(&payload).unwrap()).is_err());
    }
}
