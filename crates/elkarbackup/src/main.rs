//! `elkarbackup`: the server binary.

mod bootstrap;
mod config;

use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use elkar_engine::{Engine, EngineError, Notifier, NullNotifier, Scheduler, SmtpNotifier, SystemRunner, keys};
use elkar_storage::Store;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[derive(Parser)]
#[command(version, about = "Centralised rsnapshot based backups")]
struct Cli {
    /// Configuration file.
    #[arg(
        short,
        long,
        env = "ELKARBACKUP_CONFIG",
        default_value = "/etc/elkarbackup/elkarbackup.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the web interface, API and backup scheduler.
    Serve {
        /// Serve only the web interface; run `tick` from cron instead.
        #[arg(long)]
        no_scheduler: bool,
    },
    /// Queue due jobs and process the queue until it is idle (cron mode).
    Tick,
    /// Apply database migrations and create default records.
    Migrate,
    /// Create an administrator. The password is read from stdin.
    CreateAdmin {
        #[arg(long)]
        username: String,
        #[arg(long)]
        email: String,
    },
    /// Create the SSH key pair used to connect to clients.
    GenerateKeypair {
        /// Replace an existing key pair.
        #[arg(long, conflicts_with = "if_missing")]
        force: bool,
        /// Keep an existing key pair and print its public key.
        #[arg(long)]
        if_missing: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    match cli.command {
        Command::Serve { no_scheduler } => serve(&config, !no_scheduler).await,
        Command::Tick => tick(&config).await,
        Command::Migrate => {
            let engine = engine(&config).await?;
            bootstrap::ensure_defaults(&engine.store, &config).await
        }
        Command::CreateAdmin { username, email } => {
            let store = connect(&config).await?;
            let password = read_password()?;
            bootstrap::create_admin(&store, &username, &email, &password).await?;
            println!("Administrator {username} created");
            Ok(())
        }
        Command::GenerateKeypair { force, if_missing } => {
            let path = config.engine().ssh_key;
            if !force && let Some(public) = keys::read_public_key(&path).await {
                if if_missing {
                    print!("{public}");
                    return Ok(());
                }
                bail!("{} already exists; pass --force to replace it", path.display());
            }
            let public = keys::generate(&SystemRunner, &path).await?;
            print!("{public}");
            Ok(())
        }
    }
}

fn read_password() -> anyhow::Result<String> {
    eprintln!("Password (read from stdin):");
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\n', '\r']).to_owned())
}

async fn connect(config: &Config) -> anyhow::Result<Store> {
    let store = Store::connect(&config.database_url, 10)
        .await
        .context("connecting to the database")?;
    store.migrate().await.context("applying migrations")?;
    Ok(store)
}

async fn engine(config: &Config) -> anyhow::Result<Engine> {
    let store = connect(config).await?;
    let notifier: Arc<dyn Notifier> = match config.smtp() {
        Some(settings) => Arc::new(SmtpNotifier::new(&settings).map_err(anyhow::Error::msg)?),
        None => Arc::new(NullNotifier),
    };
    let engine_config = config.engine();
    for dir in [
        &engine_config.tmp_dir,
        &engine_config.log_dir,
        &engine_config.scripts_dir,
    ] {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(Engine {
        store,
        config: Arc::new(engine_config),
        runner: Arc::new(SystemRunner),
        notifier,
    })
}

async fn serve(config: &Config, with_scheduler: bool) -> anyhow::Result<()> {
    let engine = engine(config).await?;
    bootstrap::ensure_defaults(&engine.store, config).await?;
    let shutdown = CancellationToken::new();

    let scheduler = if with_scheduler {
        match Scheduler::start(engine.clone()).await {
            Ok(mut scheduler) => {
                let token = shutdown.clone();
                Some(tokio::spawn(async move { scheduler.run_forever(token).await }))
            }
            Err(EngineError::SchedulerBusy) => {
                tracing::warn!("another scheduler holds the lock; serving the web interface only");
                None
            }
            Err(err) => return Err(err.into()),
        }
    } else {
        None
    };

    let state = elkar_web::AppState::new(engine, config.web());
    let app = elkar_web::router(state).layer(tower_http::trace::TraceLayer::new_for_http());
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    tracing::info!(address = %config.listen, "listening");

    let token = shutdown.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            wait_for_signal().await;
            token.cancel();
        })
        .await?;
    if let Some(handle) = scheduler {
        tracing::info!("waiting for running jobs to stop");
        handle.await?;
    }
    Ok(())
}

async fn tick(config: &Config) -> anyhow::Result<()> {
    let engine = engine(config).await?;
    let mut scheduler = match Scheduler::start(engine).await {
        Ok(scheduler) => scheduler,
        Err(EngineError::SchedulerBusy) => {
            tracing::info!("scheduler already running, tick skipped");
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    scheduler.enqueue_scheduled(chrono::Local::now()).await?;
    scheduler.purge_old_logs().await?;
    scheduler.run_until_idle().await?;
    Ok(())
}

async fn wait_for_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}
