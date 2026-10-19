#[path = "cli_setup.rs"]
mod cli_setup;
#[path = "cli_tui.rs"]
mod cli_tui;
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use std::{io::IsTerminal, path::PathBuf, sync::Arc};
use werk_worker::{
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

#[derive(Clone, Parser)]
#[command(
    name = env!("CARGO_BIN_NAME"),
    version,
    about = "werkWorker general-purpose local agent engine"
)]
struct Cli {
    /// Open the interactive connection setup, even with an explicit configuration.
    #[arg(long)]
    setup: bool,
    #[arg(long)]
    store: Option<PathBuf>,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long)]
    grants: Option<PathBuf>,
    /// Host-owned inference and subagent limits JSON.
    #[arg(long)]
    execution_config: Option<PathBuf>,
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
    command: Option<Command>,
}
#[derive(Clone, Subcommand)]
enum Command {
    /// Interactive agent UI (the default when no command is supplied).
    Tui {
        input: Option<String>,
        #[arg(long, default_value = "general-readonly")]
        profile: String,
        #[arg(long)]
        demo: bool,
        #[arg(long, requires = "run_id")]
        session: Option<String>,
        #[arg(long, requires = "session")]
        run_id: Option<String>,
    },
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
#[derive(Clone, Subcommand)]
enum SessionCommand {
    List,
    Inspect { id: String },
    Export { id: String },
}
#[derive(Clone, Subcommand)]
enum ListCommand {
    List,
}
#[derive(Clone, Subcommand)]
enum InspectCommand {
    List,
    Inspect { name: String },
}
#[derive(Clone, Subcommand)]
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
async fn execute(mut cli: Cli) -> Result<()> {
    loop {
        if !execute_once(cli.clone()).await? {
            return Ok(());
        }
        cli.setup = true;
        if let Some(Command::Tui {
            input,
            session,
            run_id,
            ..
        }) = &mut cli.command
        {
            *input = None;
            *session = None;
            *run_id = None;
        }
    }
}
async fn execute_once(cli: Cli) -> Result<bool> {
    let command = cli.command.unwrap_or(Command::Tui {
        input: None,
        profile: "general-readonly".into(),
        demo: false,
        session: None,
        run_id: None,
    });
    if matches!(&command, Command::Tui { .. })
        && (!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal())
    {
        return Err(WorkerError::new(
            "terminal_required",
            "TUI requires an interactive terminal; use run or rpc for pipes",
        ));
    }
    if let Command::Profiles { action } = &command {
        match action {
            InspectCommand::List => println!("{}", serde_json::to_string(&AgentProfile::names())?),
            InspectCommand::Inspect { name } => println!(
                "{}",
                serde_json::to_string_pretty(&AgentProfile::bundled(name)?)?
            ),
        }
        return Ok(false);
    }
    if let Command::Tools { .. } = &command {
        println!(
            "{}",
            serde_json::to_string_pretty(&tools::ToolRegistry::builtins()?.definitions())?
        );
        return Ok(false);
    }
    if let Command::Skills { action } = &command {
        let skills = skills::SkillRegistry::bundled()?;
        match action {
            InspectCommand::List => println!("{}", serde_json::to_string_pretty(&skills.list())?),
            InspectCommand::Inspect { name } => {
                println!("{}", serde_json::to_string_pretty(skills.get(name)?)?)
            }
        }
        return Ok(false);
    }
    if cli.setup && !matches!(&command, Command::Tui { .. }) {
        return Err(WorkerError::new(
            "setup_requires_tui",
            "--setup requires the interactive TUI",
        ));
    }
    let sandbox = Sandbox {
        bubblewrap: cli.bubblewrap,
        ..Default::default()
    };
    if matches!(&command, Command::Sandbox { .. }) {
        println!("{}", sandbox.doctor().await?);
        return Ok(false);
    }
    let execution = if let Some(path) = &cli.execution_config {
        serde_json::from_slice::<parallel::ExecutionConfig>(&tokio::fs::read(path).await?)?
    } else {
        parallel::ExecutionConfig::default()
    };
    execution.validate()?;
    let mut setup_limits = Limits::default();
    let mut setup_profile = None;
    let mut setup_access = project::Access::None;
    let project = if matches!(&command, Command::Tui { .. } | Command::Run { .. }) {
        project::Project::load(&cli.workspace).await?
    } else {
        project::Project::default()
    };
    let explicit_profile = match &command {
        Command::Tui { profile, .. } if profile != "general-readonly" => Some(profile.as_str()),
        _ => None,
    };
    let use_setup = matches!(&command, Command::Tui { demo: false, .. })
        && cli.script.is_none()
        && (cli.setup || cli.werk_config.is_none() || cli.secret_env.is_none());
    let provider: Arc<dyn ModelProvider> = if cli.setup || use_setup {
        let seed = if let Some(path) = &cli.werk_config {
            Some(serde_json::from_slice(&tokio::fs::read(path).await?)?)
        } else {
            None
        };
        let Some(connected) = cli_setup::show(
            seed,
            cli.secret_env.clone(),
            cli.grants.is_some(),
            &cli.workspace,
            &project,
            explicit_profile,
        )
        .await?
        else {
            return Ok(false);
        };
        setup_limits = connected.limits;
        setup_profile = Some(connected.profile);
        setup_access = connected.access;
        connected.provider
    } else if let Some(path) = &cli.werk_config {
        let config: WerkConfig = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        let secret = cli
            .secret_env
            .as_deref()
            .map(|r| EnvironmentSecrets.resolve(r))
            .transpose()?
            .flatten();
        let provider = WerkProvider::connect(config, secret).await?;
        if matches!(command, Command::Diagnose) {
            println!(
                "{}",
                serde_json::json!({"runtime_capabilities":provider.runtime_capabilities,"discovery":provider.discovery,"context_tokens":provider.capabilities().context_tokens,"context_limit_source":provider.context_limit_source,"inference_concurrency":provider.inference_concurrency,"effective_model_requests":execution.effective_requests(&provider.inference_concurrency),"execution":execution})
            );
            return Ok(false);
        }
        Arc::new(provider)
    } else if let Some(path) = &cli.script {
        Arc::new(ScriptedProvider::new(serde_json::from_slice(
            &tokio::fs::read(path).await?,
        )?))
    } else if matches!(
        &command,
        Command::Run { demo: true, .. } | Command::Tui { demo: true, .. }
    ) {
        Arc::new(ScriptedProvider::new(vec![ScriptedTurn::text(
            "werkWorker offline demo completed. This is a scripted infrastructure check, not a model response.",
        )]))
    } else if matches!(&command, Command::Sessions { .. }) {
        Arc::new(ScriptedProvider::default())
    } else if matches!(&command, Command::Tui { .. }) {
        return cli_tui::show(None, None, None, Limits::default()).await;
    } else {
        return Err(WorkerError::new(
            "provider_required",
            "Select --werk-config or --script; run --demo is explicitly offline",
        ));
    };
    let policy = if let Some(path) = cli.grants {
        serde_json::from_slice(&tokio::fs::read(path).await?)?
    } else {
        setup_access.policy()
    };
    let store_path = match cli.store {
        Some(path) => path,
        None => default_store_path()?,
    };
    let store = Arc::new(SqliteStore::open(&store_path).await?);
    let artifacts = ArtifactStore::open(
        store_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("artifacts"),
    )
    .await?;
    let engine = WorkerEngine::builder(provider)
        .execution(execution)
        .store(store)
        .sandbox(sandbox)
        .artifacts(artifacts)
        .build()
        .await?;
    let mut configure = false;
    let result = match command {
        Command::Tui {
            input,
            profile,
            session: session_id,
            run_id,
            ..
        } => {
            let (session, resume) = if let (Some(session_id), Some(run_id)) = (session_id, run_id) {
                let session = engine.open_session(&session_id).await?;
                let run = engine.resume_run(&session_id, &run_id).await?;
                (session, Some(run))
            } else {
                (
                    engine
                        .create_session(
                            match setup_profile {
                                Some(profile) => profile,
                                None => {
                                    project
                                        .profile(
                                            if profile == "general-readonly" {
                                                project
                                                    .config
                                                    .profile
                                                    .as_deref()
                                                    .unwrap_or(&profile)
                                            } else {
                                                &profile
                                            },
                                            &cli.workspace,
                                            cli_setup::settings_path()?
                                                .parent()
                                                .expect("config parent"),
                                        )
                                        .await?
                                }
                            },
                            cli.workspace,
                            policy,
                            project.metadata(),
                        )
                        .await?,
                    None,
                )
            };
            cli_tui::show(Some(session), input, resume, setup_limits)
                .await
                .map(|again| {
                    configure = again;
                })
        }
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
                    project
                        .profile(
                            if profile == "general-readonly" {
                                project.config.profile.as_deref().unwrap_or(&profile)
                            } else {
                                &profile
                            },
                            &cli.workspace,
                            cli_setup::settings_path()?.parent().expect("config parent"),
                        )
                        .await?,
                    cli.workspace,
                    policy,
                    project.metadata(),
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
    result?;
    Ok(configure)
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

fn default_store_path() -> Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    base.map(|base| base.join("werk-worker/sessions.sqlite"))
        .ok_or_else(|| {
            WorkerError::new(
                "invalid_store",
                "Set --store or configure the user's state directory",
            )
        })
}
