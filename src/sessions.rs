//! Atomic session state + event persistence. SQLite has one exclusive host owner.
use crate::core::*;
use async_trait::async_trait;
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    path::Path,
    sync::{Arc, Mutex},
};

#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Host-control files must never be included in an agent workspace mount.
    fn protected_paths(&self) -> Vec<std::path::PathBuf> {
        vec![]
    }
    async fn load(&self, id: &str) -> Result<SessionData>;
    async fn list(&self) -> Result<Vec<String>>;
    /// expected=None means insert; updates use compare-and-swap on revision.
    async fn save(&self, data: &SessionData, expected: Option<u64>) -> Result<()>;
    /// Atomically remove exactly these revisions, or leave every row unchanged.
    async fn delete(&self, _revisions: &[(String, u64)]) -> Result<()> {
        Err(WorkerError::new(
            "unsupported",
            "This session store does not support deletion",
        ))
    }
    async fn close(&self) -> Result<()>;
}
#[derive(Default)]
pub struct InMemoryStore {
    data: Mutex<BTreeMap<String, SessionData>>,
}
#[async_trait]
impl SessionStore for InMemoryStore {
    async fn delete(&self, revisions: &[(String, u64)]) -> Result<()> {
        let mut all = self.data.lock().unwrap();
        for (id, revision) in revisions {
            if all.get(id).map(|s| s.revision) != Some(*revision) {
                return Err(WorkerError::new(
                    "store_conflict",
                    "Session changed; refresh before deleting",
                ));
            }
        }
        for (id, _) in revisions {
            all.remove(id);
        }
        Ok(())
    }
    async fn load(&self, id: &str) -> Result<SessionData> {
        self.data
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| WorkerError::new("not_found", "Session not found"))
    }
    async fn list(&self) -> Result<Vec<String>> {
        Ok(self.data.lock().unwrap().keys().cloned().collect())
    }
    async fn save(&self, data: &SessionData, expected: Option<u64>) -> Result<()> {
        let mut all = self.data.lock().unwrap();
        if all.get(&data.id).map(|s| s.revision) != expected {
            return Err(WorkerError::new(
                "store_conflict",
                "Session revision conflict",
            ));
        }
        all.insert(data.id.clone(), data.clone());
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}
pub struct SqliteStore {
    inner: Arc<Mutex<Option<(Connection, File)>>>,
    path: std::path::PathBuf,
}
fn db_error(_: rusqlite::Error) -> WorkerError {
    WorkerError::new(
        "store_error",
        "SQLite operation failed; store was not reset",
    )
}
fn validate(data: SessionData) -> Result<SessionData> {
    if data.schema_version != 1
        || data.events.iter().enumerate().any(|(i, e)| {
            e.schema_version != 1 || e.cursor != i as u64 + 1 || e.session_id != data.id
        })
    {
        return Err(WorkerError::new(
            "store_version",
            "Unsupported or inconsistent session/event schema",
        ));
    }
    Ok(data)
}
impl SqliteStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let protected_path = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir()?.join(&path)
        };
        let pair = tokio::task::spawn_blocking(move || -> Result<_> {
            if path == Path::new(":memory:") { return Err(WorkerError::new("invalid_store", "Use InMemoryStore for volatile sessions")); }
            let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
            std::fs::create_dir_all(parent)?;
            let mut opts = OpenOptions::new(); opts.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; opts.mode(0o600).custom_flags(libc::O_NOFOLLOW); }
            let lock = opts.open(path.with_extension("owner.lock"))?;
            lock.try_lock_exclusive().map_err(|_| WorkerError::new("store_owned", "Another host owns this store"))?;
            // Create the DB with private permissions before SQLite opens its journal.
            let db_file = opts.open(&path)?; drop(db_file);
            let mut conn = Connection::open(&path).map_err(db_error)?;
            let version: u32 = conn.pragma_query_value(None,"user_version", |r| r.get(0)).map_err(db_error)?;
            if version > 1 { return Err(WorkerError::new("store_version", "Store schema is newer than this worker")); }
            if version == 0 {
                let tx = conn.transaction().map_err(db_error)?;
                tx.execute_batch("CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, revision INTEGER NOT NULL, data TEXT NOT NULL); PRAGMA user_version=1;").map_err(db_error)?;
                tx.commit().map_err(db_error)?;
            }
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=1000;").map_err(db_error)?;
            let integrity: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0)).map_err(db_error)?;
            if integrity != "ok" { return Err(WorkerError::new("store_corrupt", "SQLite integrity check failed")); }
            Ok((conn, lock))
        }).await.map_err(|_| WorkerError::new("store_error", "Store worker failed"))??;
        Ok(Self {
            inner: Arc::new(Mutex::new(Some(pair))),
            path: tokio::fs::canonicalize(protected_path).await?,
        })
    }
    async fn with<T: Send + 'static>(
        &self,
        op: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner
                .lock()
                .map_err(|_| WorkerError::new("store_error", "Store poisoned"))?;
            let (conn, _) = guard
                .as_mut()
                .ok_or_else(|| WorkerError::new("closed", "Store is closed"))?;
            op(conn)
        })
        .await
        .map_err(|_| WorkerError::new("store_error", "Store worker failed"))?
    }
}
#[async_trait]
impl SessionStore for SqliteStore {
    async fn delete(&self, revisions: &[(String, u64)]) -> Result<()> {
        let revisions = revisions.to_vec();
        self.with(move |c| {
            let tx = c.transaction().map_err(db_error)?;
            for (id, revision) in revisions {
                if tx
                    .execute(
                        "DELETE FROM sessions WHERE id=? AND revision=?",
                        params![id, revision],
                    )
                    .map_err(db_error)?
                    != 1
                {
                    return Err(WorkerError::new(
                        "store_conflict",
                        "Session changed; refresh before deleting",
                    ));
                }
            }
            tx.commit().map_err(db_error)
        })
        .await
    }
    fn protected_paths(&self) -> Vec<std::path::PathBuf> {
        vec![self.path.clone()]
    }
    async fn load(&self, id: &str) -> Result<SessionData> {
        let id = id.to_owned();
        self.with(move |c| {
            let json: Option<String> = c
                .query_row("SELECT data FROM sessions WHERE id=?", [id], |r| r.get(0))
                .optional()
                .map_err(db_error)?;
            validate(
                serde_json::from_str(
                    &json.ok_or_else(|| WorkerError::new("not_found", "Session not found"))?,
                )
                .map_err(|_| WorkerError::new("store_corrupt", "Invalid stored session"))?,
            )
        })
        .await
    }
    async fn list(&self) -> Result<Vec<String>> {
        self.with(|c| {
            let mut q = c
                .prepare("SELECT id FROM sessions ORDER BY id")
                .map_err(db_error)?;
            q.query_map([], |r| r.get(0))
                .map_err(db_error)?
                .collect::<std::result::Result<_, _>>()
                .map_err(db_error)
        })
        .await
    }
    async fn save(&self, data: &SessionData, expected: Option<u64>) -> Result<()> {
        let data = data.clone();
        self.with(move |c| {
            let json = serde_json::to_string(&data)?;
            if json.len() > 64 * 1024 * 1024 {
                return Err(WorkerError::new("session_limit", "Session exceeds 64 MiB"));
            }
            let tx = c.transaction().map_err(db_error)?;
            let changed = if let Some(revision) = expected {
                tx.execute(
                    "UPDATE sessions SET revision=?, data=? WHERE id=? AND revision=?",
                    params![data.revision, json, data.id, revision],
                )
                .map_err(db_error)?
            } else {
                tx.execute(
                    "INSERT OR IGNORE INTO sessions (id,revision,data) VALUES (?,?,?)",
                    params![data.id, data.revision, json],
                )
                .map_err(db_error)?
            };
            if changed != 1 {
                return Err(WorkerError::new(
                    "store_conflict",
                    "Session revision conflict",
                ));
            }
            tx.commit().map_err(db_error)
        })
        .await
    }
    async fn close(&self) -> Result<()> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut guard = inner.lock().unwrap();
            if let Some((conn, lock)) = guard.take() {
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                    .map_err(db_error)?;
                conn.close().map_err(|(_, e)| db_error(e))?;
                FileExt::unlock(&lock)?;
            }
            Ok(())
        })
        .await
        .map_err(|_| WorkerError::new("store_error", "Store cleanup failed"))?
    }
}
