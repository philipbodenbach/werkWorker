#[path = "cli_config.rs"]
mod cli_config;
#[path = "cli_effort.rs"]
mod cli_effort;
#[path = "cli_setup.rs"]
mod cli_setup;
#[path = "cli_start.rs"]
mod cli_start;
#[path = "cli_transcript.rs"]
mod cli_transcript;
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
    /// Run without aggregate step/request/time cutoffs (default in interactive mode).
    #[arg(long, conflicts_with = "limits")]
    continuous: bool,
    /// Explicit run budgets JSON; overrides the interactive continuous default.
    #[arg(long)]
    limits: Option<PathBuf>,
    /// Legacy option, migrated to adaptive for new runs.
    #[arg(long, hide = true, value_parser = ["classic", "minimal", "adaptive"])]
    harness: Option<String>,
    /// Optional completion criteria and explicit host guidance JSON.
    #[arg(long, conflicts_with = "harness")]
    harness_config: Option<PathBuf>,
    /// Bubblewrap executable; defaults to the system or per-user installation.
    #[arg(long)]
    bubblewrap: Option<PathBuf>,
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
        /// Open a saved conversation; optionally resume a specific interrupted run.
        #[arg(long)]
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
    Inspect {
        id: String,
    },
    Export {
        id: String,
    },
    /// Rename a saved conversation (1..80 characters).
    Rename {
        id: String,
        title: String,
    },
    /// Delete a conversation and its subagent sessions; keep workspace files.
    Delete {
        id: String,
        #[arg(long, short = 'y')]
        yes: bool,
    },
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
    if let Command::Sessions { action } = &command {
        manage_sessions(cli.store.clone().unwrap_or(default_store_path()?), action).await?;
        return Ok(false);
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
        bubblewrap: cli
            .bubblewrap
            .unwrap_or_else(|| Sandbox::default().bubblewrap),
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
    let mut reuse_project_access = true;
    let explicit_policy = cli.grants.is_some();
    let mut setup_access = if matches!(&command, Command::Tui { .. }) {
        project::Access::Ask
    } else {
        project::Access::None
    };
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
            cli.setup,
        )
        .await?
        else {
            return Ok(false);
        };
        setup_limits = connected.limits;
        setup_profile = Some(connected.profile);
        setup_access = connected.access;
        reuse_project_access = connected.reuse_project_access;
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
        return cli_tui::show(None, None, None, Limits::default())
            .await
            .map(|exit| matches!(exit, cli_tui::Exit::Setup));
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
    if let Some(path) = &cli.limits {
        setup_limits = serde_json::from_slice(&tokio::fs::read(path).await?)?;
    } else if cli.continuous || matches!(&command, Command::Tui { .. }) {
        setup_limits.remove_run_cutoffs();
    }
    setup_limits.validate()?;
    let store = Arc::new(SqliteStore::open(&store_path).await?);
    let artifacts = ArtifactStore::open(
        store_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("artifacts"),
    )
    .await?;
    let harness = if let Some(path) = &cli.harness_config {
        serde_json::from_slice(&tokio::fs::read(path).await?)?
    } else if cli.harness.as_deref() == Some("minimal") {
        harness::HarnessConfig::minimal()
    } else if cli.harness.as_deref() == Some("classic") {
        harness::HarnessConfig {
            mode: harness::HarnessMode::Classic,
            ..Default::default()
        }
    } else {
        harness::HarnessConfig::default()
    };
    let engine = WorkerEngine::builder(provider)
        .harness(harness)
        .execution(execution)
        .store(store.clone())
        .sandbox(sandbox)
        .artifacts(artifacts)
        .build()
        .await?;
    let mut configure = false;
    let result = match command {
        Command::Tui {
            mut input,
            profile,
            session: session_id,
            run_id,
            ..
        } => {
            let profile = match setup_profile {
                Some(profile) => profile,
                None => {
                    project
                        .scoped_profile(
                            if profile == "general-readonly" {
                                project.config.profile.as_deref().unwrap_or(&profile)
                            } else {
                                &profile
                            },
                            &cli.workspace,
                            cli_setup::settings_path()?.parent().expect("config parent"),
                        )
                        .await?
                }
            };
            let mut choice = match session_id {
                Some(id) => cli_start::SessionChoice::Previous(id),
                None if input.is_none() && !cli.setup => {
                    cli_start::sessions(store.as_ref(), &engine, &cli.workspace, false).await?
                }
                None => cli_start::SessionChoice::New,
            };
            let mut explicit_run = run_id;
            let mut current: Option<SessionHandle> = None;
            let mut first_new_session = true;
            loop {
                let session = match choice {
                    cli_start::SessionChoice::Back => match current.take() {
                        Some(session) => session,
                        None => break Ok(()),
                    },
                    cli_start::SessionChoice::New => {
                        // Reload defaults after /sessions as well as after restart.
                        let project = project::Project::load(&cli.workspace).await?;
                        let mut next_profile = if first_new_session {
                            profile.clone()
                        } else {
                            project
                                .scoped_profile(
                                    project.config.profile.as_deref().unwrap_or(&profile.name),
                                    &cli.workspace,
                                    cli_setup::settings_path()?.parent().expect("config parent"),
                                )
                                .await?
                        };
                        let next_policy =
                            if !explicit_policy && (!first_new_session || reuse_project_access) {
                                cli_config::load_policy(&cli.workspace)
                                    .await?
                                    .unwrap_or_else(|| policy.clone())
                            } else {
                                policy.clone()
                            };
                        if next_policy.request_approvals {
                            if project.config.tools.is_none() {
                                next_profile.tools =
                                    engine.tools().into_iter().map(|t| t.name).collect();
                            }
                            if project.config.skills.is_none() {
                                next_profile.skills =
                                    engine.skills().into_iter().map(|s| s.name).collect();
                            }
                        }
                        engine
                            .create_session(
                                next_profile,
                                &cli.workspace,
                                next_policy,
                                project.scoped_metadata(),
                            )
                            .await?
                    }
                    cli_start::SessionChoice::Previous(ref id) => {
                        match engine.open_session(id).await {
                            Ok(session) => session,
                            Err(error) => {
                                if cli_start::choose(
                                    "Cannot open session",
                                    &error.to_string(),
                                    &["Back to sessions".into()],
                                )
                                .await?
                                .is_none()
                                {
                                    break Ok(());
                                }
                                choice = cli_start::sessions(
                                    store.as_ref(),
                                    &engine,
                                    &cli.workspace,
                                    true,
                                )
                                .await?;
                                continue;
                            }
                        }
                    }
                };
                let data = session.snapshot().await?;
                let mut active = match data.runs.last() {
                    Some(run) => Some(session.run(&run.id).await?),
                    None => None,
                };
                if let Some(run_id) = explicit_run.take() {
                    active = Some(engine.resume_run(&data.id, &run_id).await?);
                } else if let Some(run) = data
                    .runs
                    .last()
                    .filter(|r| r.status == RunStatus::Interrupted)
                {
                    let mut detail = format!("{}\nThe previous run was interrupted.", run.input);
                    while let Some(0) = cli_start::choose(
                        "Interrupted session",
                        &detail,
                        &[
                            "Resume interrupted run".into(),
                            "Open history without resuming".into(),
                        ],
                    )
                    .await?
                    {
                        match engine.resume_run(&data.id, &run.id).await {
                            Ok(handle) => {
                                active = Some(handle);
                                break;
                            }
                            Err(error) => detail = error.to_string(),
                        }
                    }
                }
                match cli_tui::show(
                    Some(session.clone()),
                    input.take(),
                    active,
                    setup_limits.clone(),
                )
                .await?
                {
                    cli_tui::Exit::Quit => break Ok(()),
                    cli_tui::Exit::Setup => {
                        configure = true;
                        break Ok(());
                    }
                    cli_tui::Exit::Sessions => {
                        first_new_session = false;
                        current = Some(session);
                        choice = cli_start::sessions(store.as_ref(), &engine, &cli.workspace, true)
                            .await?;
                        if let Some(previous) = &current
                            && previous.snapshot().await.is_err()
                        {
                            current = None;
                        }
                    }
                }
            }
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
                        .scoped_profile(
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
                    project.scoped_metadata(),
                )
                .await?;
            let run = session
                .submit(input, &id("command"), setup_limits, None)
                .await?;
            display(&run).await
        }
        Command::Resume { session_id, run_id } => {
            let run = engine.resume_run(&session_id, &run_id).await?;
            display(&run).await
        }
        _ => Ok(()),
    };
    engine.close().await?;
    result?;
    Ok(configure)
}
async fn manage_sessions(path: PathBuf, action: &SessionCommand) -> Result<()> {
    use werk_worker::sessions::SessionStore;
    let store = Arc::new(SqliteStore::open(path).await?);
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![])))
        .store(store.clone())
        .build()
        .await?;
    let result: Result<()> = async {
        match action {
            SessionCommand::List => println!("{}", serde_json::to_string_pretty(&store.list().await?)?),
            SessionCommand::Inspect { id } | SessionCommand::Export { id } => {
                validate_id(id)?;
                println!("{}", serde_json::to_string_pretty(&store.load(id).await?)?);
            }
            SessionCommand::Rename { id, title } => {
                engine.rename_session(id, title).await?;
                println!("{}", serde_json::json!({"session_id":id,"title":title.trim()}));
            }
            SessionCommand::Delete { id, yes } => {
                validate_id(id)?;
                let data = store.load(id).await?;
                if !yes {
                    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                        return Err(WorkerError::new("confirmation_required", "Deletion requires --yes outside an interactive terminal"));
                    }
                    let detail = format!("{}\n{}\nDelete conversation history, images and subagent sessions permanently? Workspace files and shared artifacts remain.", data.title.as_deref().unwrap_or("Saved conversation"), id);
                    if cli_start::choose("Delete session?", &detail, &["Cancel".into(), "Delete permanently".into()]).await? != Some(1) {
                        println!("{}", serde_json::json!({"deleted":false}));
                        return Ok(());
                    }
                }
                println!("{}", serde_json::json!({"deleted":engine.delete_session(id).await?}));
            }
        }
        Ok(())
    }.await;
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
