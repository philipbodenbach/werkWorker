//! werkWorker: explicit, durable, policy-controlled agent execution.
pub mod agents;
pub mod api;
pub mod attachments;
mod completion;
pub mod context;
pub mod core;
pub mod harness;
pub mod parallel;
pub mod policy;
pub mod profiles;
pub mod project;
pub mod providers;
mod reasoning;
pub mod runtime;
pub mod sessions;
pub mod skills;
pub mod tools;
pub mod transport;

pub use api::{EngineBuilder, RunHandle, SessionHandle, WorkerEngine};
pub use core::*;
