//! Common native filesystem contract. These tests use actual files, not a
//! platform-conditional assertion that unavailable isolation counts as success.
use werk_worker::{hash, runtime::Workspace};

#[tokio::test]
async fn native_read_write_list_and_optimistic_edit() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("notes with spaces")).unwrap();
    let workspace = Workspace::open(root.path()).await.unwrap();
    workspace
        .write("notes with spaces/Grüße.txt", b"first".to_vec(), None)
        .await
        .unwrap();
    assert_eq!(
        workspace
            .read("notes with spaces/Grüße.txt", 100)
            .await
            .unwrap(),
        b"first"
    );
    assert_eq!(
        workspace.list("notes with spaces", 10).await.unwrap(),
        ["Grüße.txt"]
    );
    assert_eq!(
        workspace
            .write("notes with spaces/Grüße.txt", b"replace".to_vec(), None)
            .await
            .unwrap_err()
            .code,
        "write_conflict"
    );
    assert_eq!(
        workspace
            .write(
                "notes with spaces/Grüße.txt",
                b"replace".to_vec(),
                Some(hash("stale"))
            )
            .await
            .unwrap_err()
            .code,
        "write_conflict"
    );
    workspace
        .write(
            "notes with spaces/Grüße.txt",
            b"second".to_vec(),
            Some(hash("first")),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(root.path().join("notes with spaces/Grüße.txt")).unwrap(),
        b"second"
    );
    assert_eq!(
        workspace
            .read("notes with spaces/Grüße.txt", 2)
            .await
            .unwrap_err()
            .code,
        "file_limit"
    );
    assert_eq!(
        workspace
            .list("notes with spaces", 0)
            .await
            .unwrap_err()
            .code,
        "listing_limit"
    );
    assert!(
        std::fs::read_dir(root.path().join("notes with spaces"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".werk-"))
    );
}

#[tokio::test]
async fn competing_create_only_operations_publish_exactly_one_complete_file() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(root.path()).await.unwrap();
    let (left, right) = tokio::join!(
        workspace.write("winner", vec![b'a'; 65536], None),
        workspace.write("winner", vec![b'b'; 65536], None),
    );
    assert_ne!(left.is_ok(), right.is_ok());
    let content = std::fs::read(root.path().join("winner")).unwrap();
    assert!(content == vec![b'a'; 65536] || content == vec![b'b'; 65536]);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn traversal_and_missing_parent_fail_without_modifying_external_control() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("sentinel"), "outside").unwrap();
    // Positive control: the host can read the external file.
    assert_eq!(
        std::fs::read_to_string(outside.path().join("sentinel")).unwrap(),
        "outside"
    );
    let workspace = Workspace::open(root.path()).await.unwrap();
    for path in [
        "../sentinel",
        "a/../../sentinel",
        "a\\..\\sentinel",
        "/sentinel",
    ] {
        assert!(workspace.read(path, 100).await.is_err(), "{path}");
        assert!(
            workspace.write(path, b"bad".to_vec(), None).await.is_err(),
            "{path}"
        );
    }
    assert_eq!(
        workspace.read("absent", 100).await.unwrap_err().code,
        "not_found"
    );
    assert_eq!(
        workspace
            .write("absent/child", vec![], None)
            .await
            .unwrap_err()
            .code,
        "not_found"
    );
    assert_eq!(
        std::fs::read_to_string(outside.path().join("sentinel")).unwrap(),
        "outside"
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_parent_and_special_file_are_rejected_with_positive_controls() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("sentinel"), "outside").unwrap();
    symlink(outside.path(), root.path().join("escape")).unwrap();
    symlink(
        outside.path().join("sentinel"),
        root.path().join("linked-file"),
    )
    .unwrap();
    let socket = std::os::unix::net::UnixListener::bind(root.path().join("socket")).unwrap();
    let workspace = Workspace::open(root.path()).await.unwrap();
    assert_eq!(
        std::fs::read(root.path().join("escape/sentinel")).unwrap(),
        b"outside"
    );
    for path in ["escape/sentinel", "linked-file", "socket"] {
        assert!(workspace.read(path, 100).await.is_err(), "{path}");
        assert!(
            workspace.write(path, b"bad".to_vec(), None).await.is_err(),
            "{path}"
        );
    }
    assert!(
        !workspace
            .list(".", 20)
            .await
            .unwrap()
            .contains(&"escape".to_string())
    );
    assert_eq!(
        std::fs::read_to_string(outside.path().join("sentinel")).unwrap(),
        "outside"
    );
    drop(socket);
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_parent_symlink_exchange_cannot_redirect_file_operations() {
    use std::{
        os::unix::fs::symlink,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let slot = root.path().join("slot");
    let parked = root.path().join("parked");
    std::fs::create_dir(&slot).unwrap();
    std::fs::write(slot.join("sentinel"), "inside").unwrap();
    std::fs::write(outside.path().join("sentinel"), "outside").unwrap();
    assert_eq!(
        std::fs::read(outside.path().join("sentinel")).unwrap(),
        b"outside"
    );
    let workspace = Workspace::open(root.path()).await.unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let external_path = outside.path().to_owned();
    let (ready, started) = std::sync::mpsc::channel();
    let exchange = std::thread::spawn(move || {
        let mut count = 0;
        while !thread_stop.load(Ordering::Relaxed) {
            std::fs::rename(&slot, &parked).unwrap();
            symlink(&external_path, &slot).unwrap();
            if count == 0 {
                ready.send(()).unwrap();
            }
            std::thread::yield_now();
            std::fs::remove_file(&slot).unwrap();
            std::fs::rename(&parked, &slot).unwrap();
            count += 1;
        }
        count
    });
    started.recv().unwrap();
    let mut observed_external = false;
    for index in 0..100 {
        if let Ok(content) = workspace.read("slot/sentinel", 100).await {
            observed_external |= content != b"inside";
        }
        // Failures during namespace exchange are permitted. A successful
        // create may only address the real directory pinned by the operation.
        let _ = workspace
            .write(&format!("slot/created-{index}"), b"inside".to_vec(), None)
            .await;
    }
    stop.store(true, Ordering::Relaxed);
    assert!(exchange.join().unwrap() > 0);
    assert!(!observed_external);
    assert_eq!(
        std::fs::read(outside.path().join("sentinel")).unwrap(),
        b"outside"
    );
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 1);
    assert!(
        std::fs::read_dir(root.path().join("slot"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".werk-"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn root_identity_detects_replacement_while_pinned_reads_keep_original_object() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("value"), "original").unwrap();
    let workspace = Workspace::open(&root).await.unwrap();
    std::fs::rename(&root, parent.path().join("moved")).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("value"), "replacement").unwrap();
    let replacement = Workspace::open(&root).await.unwrap();
    assert_ne!(workspace.identity, replacement.identity);
    assert_eq!(workspace.read("value", 100).await.unwrap(), b"original");
    assert_eq!(
        workspace.revalidate().await.unwrap_err().code,
        "workspace_conflict"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn windows_namespaces_streams_reserved_names_and_root_rename_are_rejected() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("normal"), "safe").unwrap();
    let workspace = Workspace::open(&root).await.unwrap();
    assert!(workspace.identity.starts_with("win-v1:"));
    let alias = Workspace::open(root.to_string_lossy().to_uppercase())
        .await
        .unwrap();
    assert_eq!(workspace.identity, alias.identity);
    for path in [
        "C:normal",
        "C:/normal",
        "//server/share",
        "\\\\?\\C:\\normal",
        "normal:secret",
        "NUL",
        "CON.txt",
        "COM1",
        "LPT9.log",
        "normal.",
        "normal ",
    ] {
        assert!(workspace.read(path, 100).await.is_err(), "{path}");
        assert!(
            workspace.write(path, b"bad".to_vec(), None).await.is_err(),
            "{path}"
        );
    }
    assert!(std::fs::rename(&root, parent.path().join("moved")).is_err());
    assert!(std::fs::rename(parent.path(), parent.path().with_extension("moved")).is_err());
    assert_eq!(workspace.read("normal", 100).await.unwrap(), b"safe");
    workspace.revalidate().await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_mutable_creation_time_cannot_change_workspace_lease_identity() {
    use std::os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    };
    use windows_sys::Win32::{
        Foundation::{FILETIME, INVALID_HANDLE_VALUE},
        Storage::FileSystem::*,
    };
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(root.path()).await.unwrap();
    let path: Vec<u16> = root
        .path()
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let raw = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_WRITE_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(raw, INVALID_HANDLE_VALUE);
    let handle = unsafe { std::fs::File::from_raw_handle(raw) };
    let altered = FILETIME {
        dwLowDateTime: 123456789,
        dwHighDateTime: 31000000,
    };
    assert_ne!(
        unsafe {
            SetFileTime(
                handle.as_raw_handle(),
                &altered,
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let reopened = Workspace::open(root.path()).await.unwrap();
    assert_eq!(reopened.identity, workspace.identity);
    workspace.revalidate().await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_reparse_junction_is_rejected_without_developer_mode() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("sentinel"), "outside").unwrap();
    let junction = root.path().join("junction");
    // This trusted test setup creates a directory junction (no symlink privilege).
    let result = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&junction)
        .arg(outside.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read(junction.join("sentinel")).unwrap(),
        b"outside"
    );
    let workspace = Workspace::open(root.path()).await.unwrap();
    assert!(workspace.read("junction/sentinel", 100).await.is_err());
    assert!(workspace.write("junction/new", vec![], None).await.is_err());
    assert!(Workspace::open(&junction).await.is_err());
    assert!(
        !workspace
            .list(".", 10)
            .await
            .unwrap()
            .contains(&"junction".to_string())
    );
    assert_eq!(
        std::fs::read(outside.path().join("sentinel")).unwrap(),
        b"outside"
    );
    std::fs::hard_link(
        outside.path().join("sentinel"),
        root.path().join("hardlink"),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(root.path().join("hardlink")).unwrap(),
        b"outside"
    );
    assert!(workspace.read("hardlink", 100).await.is_err());
    assert!(
        workspace
            .write("hardlink", b"bad".to_vec(), Some(hash("outside")))
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(outside.path().join("sentinel")).unwrap(),
        b"outside"
    );
}
