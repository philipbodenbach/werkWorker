//! Minimal external-host pattern: no CLI imports.
use futures_util::StreamExt;
use std::sync::Arc;
use werk_worker::{
    policy::HostPolicy,
    profiles::AgentProfile,
    providers::{ScriptedProvider, ScriptedTurn},
    *,
};

#[tokio::main]
async fn main() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
        "A library consumer started this run.",
    )]));
    let engine = WorkerEngine::builder(provider).build().await?;
    let mut profile = AgentProfile::bundled("general-readonly")?;
    profile.tools.clear();
    let session = engine
        .create_session(
            profile,
            workspace.path(),
            HostPolicy::default(),
            serde_json::Value::Null,
        )
        .await?;
    let run = session
        .submit("Say hello", "consumer-1", Limits::default(), None)
        .await?;
    let mut events = run.events(0);
    while let Some(event) = events.next().await {
        println!("{}", serde_json::to_string(&event?)?);
    }
    assert_eq!(run.wait().await?.status, RunStatus::Completed);
    engine.close().await
}
