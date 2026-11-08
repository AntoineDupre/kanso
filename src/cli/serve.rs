use std::future::IntoFuture;
use std::path::PathBuf;
use std::time::Duration;

use tracing_subscriber::EnvFilter;

use super::CliError;
use crate::auth::AuthConfig;
use crate::config::Config;
use crate::db;
use crate::server;
use crate::web;

pub fn run(port: Option<u16>, data_dir: Option<PathBuf>) -> Result<(), CliError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| CliError::Startup(format!("failed to start the runtime: {error}")))?;
    runtime.block_on(serve(port, data_dir))
}

async fn serve(port: Option<u16>, data_dir: Option<PathBuf>) -> Result<(), CliError> {
    let mut config = Config::from_env().map_err(|error| startup(error.to_string()))?;
    if let Some(data_dir) = data_dir {
        config.data_dir = data_dir;
    }
    if let Some(port) = port {
        config.bind.set_port(port);
    }
    init_tracing(&config.log)?;

    std::fs::create_dir_all(&config.data_dir).map_err(|error| {
        startup(format!(
            "failed to create data directory {}: {error}",
            config.data_dir.display()
        ))
    })?;
    let db_path = config.db_path();
    let mut connection = db::open(&db_path).map_err(|error| {
        startup(format!(
            "failed to open database {}: {error}",
            db_path.display()
        ))
    })?;
    let schema_version = db::migrate(&mut connection).map_err(|error| {
        startup(format!(
            "failed to migrate database {}: {error}",
            db_path.display()
        ))
    })?;
    let database = db::Database::from_connection(db_path.clone(), connection)
        .map_err(|error| startup(error.to_string()))?;
    let auth = AuthConfig::load(
        config.password(),
        config.public_url.as_deref(),
        &config.data_dir,
    )
    .map_err(|error| startup(error.to_string()))?;

    let web_dir = web::default_web_dir();
    if !web_dir.join("index.html").is_file() {
        return Err(startup(format!(
            "web assets not found: expected {}",
            web_dir.join("index.html").display()
        )));
    }
    tracing::info!(
        bind = %config.bind,
        database = %db_path.display(),
        web = %web_dir.display(),
        schema_version,
        "starting kanso"
    );

    let state = server::AppState::new(database, web_dir, auth);
    crate::api::secret::spawn_cleanup(state.clone());
    state.set_openchamber(config.openchamber.as_ref().map(|settings| {
        crate::adapter::openchamber::OpenChamber::new(settings.url(), settings.token())
    }));
    crate::api::integration::load(&state).map_err(startup)?;
    crate::adapter::poller::spawn(state.clone());
    let github_auth = state.github_auth.clone();
    let app = server::router(state);
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|error| startup(format!("failed to bind {}: {error}", config.bind)))?;
    tracing::info!(
        address = %listener
            .local_addr()
            .map_err(|error| startup(format!("failed to read the local address: {error}")))?,
        "kanso listening"
    );

    let (shutdown_started_tx, shutdown_started_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown = async move {
        server::shutdown_signal().await;
        let _ = shutdown_started_tx.send(());
        github_auth.shutdown().await;
    };
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .into_future();
    tokio::pin!(server);

    tokio::select! {
        result = &mut server => {
            result.map_err(|error| startup(format!("server error: {error}")))?;
        }
        () = async {
            let _ = shutdown_started_rx.await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        } => {
            tracing::warn!("graceful shutdown timed out; closing remaining connections");
        }
    }

    tracing::info!("kanso stopped");
    Ok(())
}

fn startup(message: impl Into<String>) -> CliError {
    CliError::Startup(message.into())
}

fn init_tracing(log: &str) -> Result<(), CliError> {
    let filter = EnvFilter::try_new(log)
        .map_err(|error| startup(format!("invalid KANSO_LOG value {log:?}: {error}")))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::io::stdout)
        .try_init()
        .map_err(|error| startup(format!("failed to initialize logging: {error}")))
}
