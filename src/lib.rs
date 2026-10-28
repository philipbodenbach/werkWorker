//! werkWorker: explicit, durable, policy-controlled agent execution.
pub mod agents;
pub mod api;
pub mod attachments;
mod browser;
mod completion;
pub mod context;
pub mod core;
mod discovery;
mod failure_progress;
mod file_io;
pub mod harness;
pub mod parallel;
pub mod policy;
pub mod profiles;
pub mod project;
pub mod providers;
mod read_progress;
mod reasoning;
mod repair_cycle;
#[cfg(test)]
mod repetition_tests;
pub mod runtime;
pub mod sessions;
pub mod skills;
pub mod tools;
pub mod transport;
mod work_cycle;
mod working_state;

pub use api::{EngineBuilder, RunHandle, SessionHandle, WorkerEngine};
pub use core::*;
