//! Native release contract, also run on Windows/macOS builders.
use werk_worker::runtime::Workspace;

#[test]
#[cfg(feature = "tui")]
fn native_commands_start_outside_the_checkout() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join(".werkworker.md"),
        "Synthetic native project instructions.\n",
    )
    .unwrap();
    std::fs::create_dir(workspace.join(".werkworker")).unwrap();
    std::fs::write(
        workspace.join(".werkworker/config.json"),
        r#"{"version":1,"profile":"general-readonly"}"#,
    )
    .unwrap();
    for (name, binary) in [
        ("werk-worker", env!("CARGO_BIN_EXE_werk-worker")),
        ("ww", env!("CARGO_BIN_EXE_ww")),
    ] {
        let store = root.path().join(format!("{name}.sqlite"));
        let run = |args: &[&str]| {
            let output = std::process::Command::new(binary)
                .arg("--store")
                .arg(&store)
                .args(args)
                .current_dir(&workspace)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{name} {args:?} failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        assert!(run(&["--help"]).contains(&format!("Usage: {name}")));
        assert!(run(&["run", "--demo", "native CLI smoke"]).contains("offline demo completed"));
        run(&["profiles", "inspect", "coding-basic"]);
        run(&["skills", "inspect", "ticket-drafting"]);
    }
}

#[tokio::test]
async fn workspace_can_be_opened_on_the_native_platform() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(root.path()).await.unwrap();
    workspace.revalidate().await.unwrap();
    assert_eq!(
        workspace.read("missing", 100).await.unwrap_err().code,
        "not_found"
    );
    workspace
        .write("file", b"native".to_vec(), None)
        .await
        .unwrap();
    assert_eq!(workspace.read("file", 100).await.unwrap(), b"native");
    assert_eq!(workspace.list(".", 100).await.unwrap(), ["file"]);
}

#[tokio::test]
async fn project_rules_custom_profile_and_preferences_use_native_workspace_files() {
    use werk_worker::{
        profiles::AgentProfile,
        project::{CONFIG, Project},
    };
    let root = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".werkworker/profiles")).unwrap();
    std::fs::write(
        root.path().join(".werkworker.md"),
        "Keep the synthetic fixture bounded.\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join(CONFIG),
        r#"{"version":1,"profile":"native-custom"}"#,
    )
    .unwrap();
    let mut profile = AgentProfile::bundled("general-readonly").unwrap();
    profile.name = "native-custom".into();
    std::fs::write(
        root.path().join(".werkworker/profiles/native-custom.toml"),
        toml::to_string(&profile).unwrap(),
    )
    .unwrap();
    let project = Project::load(root.path()).await.unwrap();
    assert_eq!(project.config.profile.as_deref(), Some("native-custom"));
    assert_eq!(project.instructions.len(), 1);
    assert!(project.instructions[0].text.contains("synthetic fixture"));
    let loaded = project
        .profile("native-custom", root.path(), global.path())
        .await
        .unwrap();
    assert_eq!(loaded.name, "native-custom");
    assert!(
        Project::profile_names(root.path(), global.path())
            .await
            .unwrap()
            .contains(&"native-custom".to_string())
    );
    let mut config = project.config.clone();
    config.model = Some("synthetic-model".into());
    project.save(root.path(), &config).await.unwrap();
    assert_eq!(
        Project::load(root.path())
            .await
            .unwrap()
            .config
            .model
            .as_deref(),
        Some("synthetic-model")
    );
}
