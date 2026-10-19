use std::sync::Arc;
use werk_worker::{*,policy::HostPolicy,profiles::AgentProfile,providers::{ScriptedProvider,ScriptedTurn}};

#[tokio::main]
async fn main()->Result<()> {
    let workspace=std::env::current_dir()?;
    let engine=WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text("External crate linked the public SDK.")]))).build().await?;
    let session=engine.create_session(AgentProfile::bundled("general-readonly")?,workspace,HostPolicy::default(),serde_json::Value::Null).await?;
    let run=session.submit("hello","consumer-command",Limits::default(),None).await?;
    println!("{}",serde_json::to_string(&run.wait().await?)?);
    engine.close().await
}
