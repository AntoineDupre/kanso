use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use kanso::cli::{Cli, Command};
use kanso::config::Config;
use kanso::db;
use kanso::error::{Error, Result};

const EXIT_OK: u8 = 0;
const EXIT_USAGE: u8 = 1;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // PLAN.md §6 uses exit code 1 for usage errors; clap defaults to 2.
            let code = if error.use_stderr() {
                EXIT_USAGE
            } else {
                EXIT_OK
            };
            let _ = error.print();
            return ExitCode::from(code);
        }
    };

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("kanso: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Serve { port, data_dir } => serve(port, data_dir).await,
    }
}

async fn serve(port: Option<u16>, data_dir: Option<PathBuf>) -> Result<()> {
    let mut config = Config::from_env()?;
    if let Some(data_dir) = data_dir {
        config.data_dir = data_dir;
    }
    if let Some(port) = port {
        config.bind.set_port(port);
    }
    init_tracing(&config.log)?;

    std::fs::create_dir_all(&config.data_dir).map_err(|error| {
        Error::msg(format!(
            "failed to create data directory {}: {error}",
            config.data_dir.display()
        ))
    })?;
    let db_path = config.db_path();
    let mut connection = db::open(&db_path).map_err(|error| {
        Error::msg(format!(
            "failed to open database {}: {error}",
            db_path.display()
        ))
    })?;
    let schema_version = db::migrate(&mut connection).map_err(|error| {
        Error::msg(format!(
            "failed to migrate database {}: {error}",
            db_path.display()
        ))
    })?;
    drop(connection);

    tracing::info!(
        bind = %config.bind,
        database = %db_path.display(),
        schema_version,
        "starting kanso"
    );

    Ok(())
}

fn init_tracing(log: &str) -> Result<()> {
    let filter = EnvFilter::try_new(log)
        .map_err(|error| Error::msg(format!("invalid KANSO_LOG value {log:?}: {error}")))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::io::stdout)
        .try_init()
        .map_err(|error| Error::msg(format!("failed to initialize logging: {error}")))
}
