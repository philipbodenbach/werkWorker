//! Host-owned admission limits and cooperative workspace access.
use crate::{core::*, providers::InferenceConcurrency};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionConfig {
    pub max_model_requests: usize,
    pub max_subagents: usize,
    pub max_children_per_run: usize,
    pub max_depth: usize,
}
impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            max_model_requests: 4,
            max_subagents: 4,
            max_children_per_run: 8,
            max_depth: 1,
        }
    }
}
impl ExecutionConfig {
    pub fn validate(&self) -> Result<()> {
        if !(1..=64).contains(&self.max_model_requests)
            || self.max_subagents > 32
            || self.max_children_per_run > 64
            || self.max_depth > 4
        {
            return Err(WorkerError::new(
                "invalid_limits",
                "Invalid inference/subagent limits",
            ));
        }
        Ok(())
    }
    pub fn effective_requests(&self, info: &InferenceConcurrency) -> usize {
        self.max_model_requests
            .min(info.max_requests.unwrap_or(1).max(1))
    }
}
#[derive(Default)]
pub(crate) struct WorkspaceAccess {
    leases: Mutex<Vec<(String, PathBuf, bool)>>,
}
pub struct WorkspaceLease {
    id: String,
    access: Arc<WorkspaceAccess>,
}
impl WorkspaceAccess {
    pub fn try_acquire(self: &Arc<Self>, path: &Path, write: bool) -> Option<Arc<WorkspaceLease>> {
        let mut leases = self.leases.lock().unwrap();
        if leases.iter().any(|(_, other, exclusive)| {
            (write || *exclusive) && (path.starts_with(other) || other.starts_with(path))
        }) {
            return None;
        }
        let id = id("lease");
        leases.push((id.clone(), path.to_owned(), write));
        Some(Arc::new(WorkspaceLease {
            id,
            access: self.clone(),
        }))
    }
}
impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        self.access
            .leases
            .lock()
            .unwrap()
            .retain(|(id, _, _)| *id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlapping_workspaces_share_reads_and_exclude_writers_until_last_lease_drops() {
        let access = Arc::new(WorkspaceAccess::default());
        let path = Path::new("workspace");
        let read = access.try_acquire(path, false).unwrap();
        let retained_by_job = read.clone();
        let nested = access.try_acquire(&path.join("child"), false).unwrap();
        assert!(access.try_acquire(path, true).is_none());
        assert!(access.try_acquire(Path::new("independent"), true).is_some());
        drop(read);
        drop(nested);
        assert!(access.try_acquire(path, true).is_none());
        drop(retained_by_job);
        let write = access.try_acquire(path, true).unwrap();
        assert!(access.try_acquire(&path.join("child"), false).is_none());
        drop(write);
        assert!(access.try_acquire(path, false).is_some());
    }
}
