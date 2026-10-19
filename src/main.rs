use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use std::{path::PathBuf, sync::Arc};
use werk_worker::{
    policy::HostPolicy,
    profiles::AgentProfile,
    providers::{
        EnvironmentSecrets, ModelProvider, ScriptedProvider, ScriptedTurn, SecretResolver,
        WerkConfig, WerkProvider,
    },
    runtime::{ArtifactStore, Sandbox},
    sessions::SqliteStore,
    transport::RpcHost,
    *,
};

#[derive(Parser)]
#[command(
    name = env!("CARGO_BIN_NAME"),
    version,
    about = "werkWorker general-purpose local agent engine"
)]
struct Cli {
    #[arg(long)]
    store: Option<PathBuf>,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long)]
    grants: Option<PathBuf>,
    #[arg(long, default_value = "/usr/bin/bwrap")]
    bubblewrap: PathBuf,
    /// Explicit Werk transport/model configuration JSON; no credentials inside.
    #[arg(long)]
    werk_config: Option<PathBuf>,
    #[arg(long)]
    secret_env: Option<String>,
    /// Explicit deterministic offline responses, never a provider fallback.
    #[arg(long)]
    script: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Run {
        input: String,
        #[arg(long, default_value = "general-readonly")]
        profile: String,
        #[arg(long)]
        demo: bool,
    },
    Resume {
        session_id: String,
        run_id: String,
    },
    Sessions {
        #[command(subcommand)]
        action: SessionCommand,
    },
    Tools {
        #[command(subcommand)]
        action: ListCommand,
    },
    Skills {
        #[command(subcommand)]
        action: InspectCommand,
    },
    Profiles {
        #[command(subcommand)]
        action: InspectCommand,
    },
    Sandbox {
        #[command(subcommand)]
        action: DoctorCommand,
    },
    Diagnose,
    Rpc,
}
#[derive(Subcommand)]
enum SessionCommand {
    List,
    Inspect { id: String },
    Export { id: String },
}
#[derive(Subcommand)]
enum ListCommand {
    List,
}
#[derive(Subcommand)]
enum InspectCommand {
    List,
    Inspect { name: String },
}
#[derive(Subcommand)]
enum DoctorCommand {
    Doctor,
}
#[tokio::main]
async fn main() {
    if let Err(error) = execute(Cli::parse()).await {
        eprintln!("{}", error);
        std::process::exit(
            if matches!(
                error.code.as_str(),
                "sandbox_unavailable" | "capability_denied"
            ) {
                3
            } else {
                1
            },
        );
    }
}
async fn execute(cli: Cli) -> Result<()> {
    if let Command::Profiles { action } = &cli.command {
        match action {
            InspectCommand::List => println!("{}", serde_json::to_string(&AgentProfile::names())?),
            InspectCommand::Inspect { name } => println!(
                "{}",
                serde_json::to_string_pretty(&AgentProfile::bundled(name)?)?
            ),
        }
        return Ok(());
    }
    if let Command::Tools { .. } = &cli.command {
        println!(
            "{}",
            serde_json::to_string_pretty(&tools::ToolRegistry::builtins()?.definitions())?
        );
        return Ok(());
    }
    if let Command::Skills { action } = &cli.command {
        let skills = skills::SkillRegistry::bundled()?;
        match action {
            InspectCommand::List => println!("{}", serde_json::to_string_pretty(&skills.list())?),
            InspectCommand::Inspect { name } => {
                println!("{}", serde_json::to_string_pretty(skills.get(name)?)?)
            }
        }
        return Ok(());
    }
    let sandbox = Sandbox {
        bubblewrap: cli.bubblewrap,
        ..Default::default()
    };
    if matches!(&cli.command, Command::Sandbox { .. }) {
        println!("{}", sandbox.doctor().await?);
        return Ok(());
    }
    let provider: Arc<dyn ModelProvider> = if let Some(path) = &cli.werk_config {
        let config: WerkConfig = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        let secret = cli
            .secret_env
            .as_deref()
            .map(|r| EnvironmentSecrets.resolve(r))
            .transpose()?
            .flatten();
        let provider = WerkProvider::connect(config, secret).await?;
        if matches!(cli.command, Command::Diagnose) {
            println!(
                "{}",
                serde_json::json!({"runtime_capabilities":provider.runtime_capabilities,"discovery":provider.discovery})
            );
            return Ok(());
        }
        Arc::new(provider)
    } else if let Some(path) = &cli.script {
        Arc::new(ScriptedProvider::new(serde_json::from_slice(
            &tokio::fs::read(path).await?,
        )?))
    } else if matches!(&cli.command, Command::Run { demo: true, .. }) {
        Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
            "werkWorker offline demo completed. This is a scripted infrastructure check, not a model response.",
        )]))
    } else if matches!(&cli.command, Command::Sessions { .. }) {
        Arc::new(ScriptedProvider::default())
    } else {
        return Err(WorkerError::new(
            "provider_required",
            "Select --werk-config or --script; run --demo is explicitly offline",
        ));
    };
    let policy = if let Some(path) = cli.grants {
        serde_json::from_slice(&tokio::fs::read(path).await?)?
    } else {
        HostPolicy::default()
    };
    let store_path = cli.store.unwrap_or_else(|| {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
            })
            .join("werk-worker/sessions.sqlite")
    });
    let store = Arc::new(SqliteStore::open(&store_path).await?);
    let artifacts = ArtifactStore::open(
        store_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("artifacts"),
    )
    .await?;
    let engine = WorkerEngine::builder(provider)
        .store(store)
        .sandbox(sandbox)
        .artifacts(artifacts)
        .build()
        .await?;
    let result = match cli.command {
        Command::Rpc => {
            RpcHost::new(engine.clone(), cli.workspace, policy)
                .serve(
                    tokio::io::BufReader::new(tokio::io::stdin()),
                    tokio::io::stdout(),
                )
                .await
        }
        Command::Run { input, profile, .. } => {
            let session = engine
                .create_session(
                    AgentProfile::bundled(&profile)?,
                    cli.workspace,
                    policy,
                    serde_json::Value::Null,
                )
                .await?;
            let run = session
                .submit(input, &id("command"), Limits::default(), None)
                .await?;
            display(&run).await
        }
        Command::Resume { session_id, run_id } => {
            let run = engine.resume_run(&session_id, &run_id).await?;
            display(&run).await
        }
        Command::Sessions { action } => {
            match action {
                SessionCommand::List => println!(
                    "{}",
                    serde_json::to_string_pretty(&engine.list_sessions().await?)?
                ),
                SessionCommand::Inspect { id } | SessionCommand::Export { id } => println!(
                    "{}",
                    serde_json::to_string_pretty(&engine.open_session(&id).await?.export().await?)?
                ),
            }
            Ok(())
        }
        _ => Ok(()),
    };
    engine.close().await?;
    result
}
async fn display(run: &RunHandle) -> Result<()> {
    eprintln!("run {}", run.id);
    let mut events = run.events(0);
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    use tokio::io::AsyncBufReadExt;
    loop {
        tokio::select! {
            _=tokio::signal::ctrl_c()=>{run.cancel("Ctrl-C",&id("cancel")).await?;},
            event=events.next()=>match event{
                None=>break,
                Some(Err(error))=>return Err(error),
                Some(Ok(event))=>match event.event{
                    EventKind::TextDelta{text,..}=>print!("{text}"),
                    EventKind::ApprovalRequested{request}=>{
                        eprintln!("\nApproval {}:\n{}\nAllow once? [y/N]",request.request_id,serde_json::to_string_pretty(&request.description)?);
                        let mut line=String::new();tokio::select!{_=tokio::signal::ctrl_c()=>{run.cancel("Ctrl-C",&id("cancel")).await?;},_=input.read_line(&mut line)=>{run.respond_to_approval(&request.request_id,if line.trim()=="y"{ApprovalDecision::AllowOnce}else{ApprovalDecision::Deny},&id("approval_command")).await?;}}
                    },
                    EventKind::InteractionRequested{request}=>{
                        eprintln!("\n{} {:?}",request.question,request.choices);let mut answer=String::new();
                        tokio::select!{_=tokio::signal::ctrl_c()=>{run.cancel("Ctrl-C",&id("cancel")).await?;},_=input.read_line(&mut answer)=>{run.respond_to_interaction(&request.request_id,answer.trim(),&id("answer_command")).await?;}}
                    },_=>{},
                }
            }
        }
    }
    let result = run.wait().await?;
    println!();
    eprintln!("{:?}", result.status);
    if let Some(error) = result.error {
        return Err(error);
    }
    Ok(())
}
