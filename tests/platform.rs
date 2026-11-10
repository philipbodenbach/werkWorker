//! Native release contract, also run on Windows/macOS builders.
use werk_worker::runtime::Workspace;

#[test]
#[cfg(feature = "tui")]
fn native_commands_start_outside_the_checkout() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
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
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(
            workspace.read("secret", 100).await.unwrap_err().code,
            "sandbox_unavailable"
        );
        assert_eq!(
            workspace.list(".", 100).await.unwrap_err().code,
            "sandbox_unavailable"
        );
        assert_eq!(
            workspace
                .write("file", vec![], None)
                .await
                .unwrap_err()
                .code,
            "sandbox_unavailable"
        );
        assert!(!root.path().join("file").exists());
    }
}
