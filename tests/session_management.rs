use serde_json::{Value, json};
use std::sync::Arc;
use werk_worker::{
    policy::HostPolicy,
    profiles::AgentProfile,
    providers::{ScriptedProvider, ScriptedTurn},
    sessions::{InMemoryStore, SessionStore, SqliteStore},
    *,
};

async fn session(engine: &WorkerEngine, workspace: &std::path::Path) -> SessionHandle {
    engine
        .create_session(
            AgentProfile::bundled("general-readonly").unwrap(),
            workspace,
            HostPolicy::default(),
            Value::Null,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn rename_delete_tree_preserves_other_sessions_files_and_invalidates_handles() {
    for sqlite in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("keep.txt"), "user file").unwrap();
        let db = root.path().join("sessions.sqlite");
        let store: Arc<dyn SessionStore> = if sqlite {
            Arc::new(SqliteStore::open(&db).await.unwrap())
        } else {
            Arc::new(InMemoryStore::default())
        };
        let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
            .store(store.clone())
            .build()
            .await
            .unwrap();
        let main = session(&engine, &workspace).await;
        let sid = main.id().await;
        engine
            .rename_session(&sid, "  Tetris – Referenz  ")
            .await
            .unwrap();
        assert_eq!(
            main.snapshot().await.unwrap().title.as_deref(),
            Some("Tetris – Referenz")
        );
        assert_eq!(
            store.load(&sid).await.unwrap().title.as_deref(),
            Some("Tetris – Referenz")
        );
        for title in [
            "".to_owned(),
            "x".repeat(81),
            "bad\ntitle".into(),
            "escape\x1b".into(),
        ] {
            assert_eq!(
                engine.rename_session(&sid, &title).await.unwrap_err().code,
                "invalid_title"
            );
        }
        // Persist a descendant as it would exist after restart, including one
        // more level. An independent fork must survive deleting its source.
        let child = session(&engine, &workspace).await;
        let child_id = child.id().await;
        let grandchild = session(&engine, &workspace).await;
        for (handle, parent) in [(&child, sid.clone()), (&grandchild, child_id.clone())] {
            let mut data = handle.snapshot().await.unwrap();
            let revision = data.revision;
            data.revision += 1;
            data.parent = Some(agents::ParentLink {
                session_id: parent,
                run_id: "run_parent".into(),
                root_session: sid.clone(),
                root_run: "run_parent".into(),
                depth: 1,
            });
            store.save(&data, Some(revision)).await.unwrap();
        }
        let other = main.fork(HostPolicy::default()).await.unwrap();
        let other_id = other.id().await;
        assert_eq!(
            engine.delete_session(&child_id).await.unwrap_err().code,
            "root_required"
        );
        let deleted = engine.delete_session(&sid).await.unwrap();
        assert_eq!(deleted.len(), 3);
        assert!(deleted.contains(&child_id));
        assert_eq!(
            engine.list_sessions().await.unwrap(),
            vec![other_id.clone()]
        );
        for handle in [&main, &child, &grandchild] {
            assert_eq!(handle.snapshot().await.unwrap_err().code, "not_found");
            assert!(
                handle
                    .submit("must not resurrect", "late", Limits::default(), None)
                    .await
                    .is_err()
            );
        }
        assert!(engine.open_session(&sid).await.is_err());
        assert_eq!(
            std::fs::read_to_string(workspace.join("keep.txt")).unwrap(),
            "user file"
        );
        engine.close().await.unwrap();
        if sqlite {
            let reopened = SqliteStore::open(&db).await.unwrap();
            assert_eq!(reopened.list().await.unwrap(), vec![other_id]);
            reopened.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn deletion_refuses_active_work_and_atomic_batches_roll_back() {
    for sqlite in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store: Arc<dyn SessionStore> = if sqlite {
            Arc::new(SqliteStore::open(root.path().join("db")).await.unwrap())
        } else {
            Arc::new(InMemoryStore::default())
        };
        let mut turn = ScriptedTurn::text("slow");
        turn.delay_ms = 30000;
        let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![turn])))
            .store(store.clone())
            .build()
            .await
            .unwrap();
        let a = session(&engine, &workspace).await;
        let b = session(&engine, &workspace).await;
        let ad = a.snapshot().await.unwrap();
        let bd = b.snapshot().await.unwrap();
        assert_eq!(
            store
                .delete(&[
                    (ad.id.clone(), ad.revision),
                    (bd.id.clone(), bd.revision + 1)
                ])
                .await
                .unwrap_err()
                .code,
            "store_conflict"
        );
        assert!(store.load(&ad.id).await.is_ok());
        assert!(store.load(&bd.id).await.is_ok());
        let run = a
            .submit("wait", "start", Limits::default(), None)
            .await
            .unwrap();
        assert_eq!(
            engine.delete_session(&ad.id).await.unwrap_err().code,
            "session_busy"
        );
        assert!(store.load(&ad.id).await.is_ok());
        run.cancel("test", "cancel").await.unwrap();
        run.wait().await.unwrap();
        // Await actor cleanup as well as the durable terminal run state.
        for _ in 0..100 {
            match engine.delete_session(&ad.id).await {
                Ok(_) => break,
                Err(e) if e.code == "session_busy" => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await
                }
                Err(e) => panic!("{e}"),
            }
        }
        assert!(store.load(&ad.id).await.is_err());
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn offline_cli_rename_export_and_confirmed_delete_without_workspace_or_provider() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("sessions.sqlite");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
        .store(Arc::new(SqliteStore::open(&db).await.unwrap()))
        .build()
        .await
        .unwrap();
    let sid = session(&engine, &workspace).await.id().await;
    engine.close().await.unwrap();
    std::fs::remove_dir(&workspace).unwrap();
    let call = |args: Vec<String>| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ww"));
        command
            .arg("--store")
            .arg(&db)
            .arg("sessions")
            .args(args)
            .stdin(std::process::Stdio::null());
        command
    };
    let out = call(vec!["rename".into(), sid.clone(), "Saved title".into()])
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);
    let out = call(vec!["export".into(), sid.clone()]).output().unwrap();
    assert!(out.status.success());
    let data: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(data["title"], "Saved title");
    let out = call(vec!["delete".into(), sid.clone()]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("confirmation_required"));
    let out = call(vec!["delete".into(), sid.clone(), "--yes".into()])
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out);
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["deleted"],
        json!([sid])
    );
    let out = call(vec!["list".into()]).output().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        json!([])
    );
}

#[tokio::test]
async fn rpc_session_deletion_requires_confirmation_and_drops_cached_handles() {
    let workspace = tempfile::tempdir().unwrap();
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
        .build()
        .await
        .unwrap();
    let mut host = transport::RpcHost::new(
        engine.clone(),
        workspace.path().into(),
        HostPolicy::default(),
    );
    let request = |method: &str, params| transport::RpcRequest {
        id: json!(1),
        method: method.into(),
        params,
    };
    host.dispatch(&request("initialize", json!({"protocol_version":1})))
        .await
        .unwrap();
    let created = host
        .dispatch(&request(
            "session.create",
            json!({"profile":"general-readonly"}),
        ))
        .await
        .unwrap();
    let id = created["session_id"].as_str().unwrap();
    host.dispatch(&request(
        "session.rename",
        json!({"session_id":id,"title":"RPC title"}),
    ))
    .await
    .unwrap();
    assert_eq!(
        host.dispatch(&request("session.export", json!({"session_id":id})))
            .await
            .unwrap()["title"],
        "RPC title"
    );
    assert_eq!(
        host.dispatch(&request("session.delete", json!({"session_id":id})))
            .await
            .unwrap_err()
            .code,
        "confirmation_required"
    );
    assert!(
        engine
            .list_sessions()
            .await
            .unwrap()
            .contains(&id.to_owned())
    );
    host.dispatch(&request(
        "session.delete",
        json!({"session_id":id,"confirm":true}),
    ))
    .await
    .unwrap();
    assert_eq!(
        host.dispatch(&request("session.export", json!({"session_id":id})))
            .await
            .unwrap_err()
            .code,
        "not_found"
    );
    engine.close().await.unwrap();
}
