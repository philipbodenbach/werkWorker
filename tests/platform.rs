//! Native release contract, also run on Windows/macOS builders.
use werk_worker::runtime::Workspace;
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
