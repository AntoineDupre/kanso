use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Serialize;

use crate::auth::AuthConfig;
use crate::db::{self, Database};
use crate::events::{EventBus, DEFAULT_HEARTBEAT};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The public health probe runs a full `quick_check`; cache its answer briefly
/// so repeated unauthenticated calls cannot saturate the blocking pool.
const HEALTH_CACHE_TTL: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub secrets: Arc<crate::secret_memory::SecretMemory>,
    /// Health-check path; normally the same file as `db`.
    pub db_path: PathBuf,
    pub web_dir: PathBuf,
    pub auth: Arc<AuthConfig>,
    pub login_limiter: Arc<crate::auth::LoginLimiter>,
    pub events: Arc<EventBus>,
    pub imports: Arc<crate::importer::Registry>,
    /// Present only when both OpenChamber environment variables are set.
    pub openchamber: Option<Arc<crate::adapter::openchamber::OpenChamber>>,
    /// Task ids with an upstream dispatch in flight, so two clicks cannot
    /// create two sessions for the same card.
    dispatch_in_flight: Arc<Mutex<HashSet<String>>>,
    health: Arc<Mutex<Option<HealthCache>>>,
    health_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Clone, Copy)]
struct HealthCache {
    checked_at: Instant,
    healthy: bool,
}

impl AppState {
    pub fn new(db: Database, web_dir: PathBuf, auth: AuthConfig) -> Self {
        Self::with_heartbeat(db, web_dir, auth, DEFAULT_HEARTBEAT)
    }

    /// Same as [`AppState::new`], with an explicit SSE heartbeat interval.
    /// Tests use a short interval instead of waiting 25 seconds.
    pub fn with_heartbeat(
        db: Database,
        web_dir: PathBuf,
        auth: AuthConfig,
        heartbeat: Duration,
    ) -> Self {
        let db_path = db.path().to_path_buf();
        let secrets = db.secrets();
        Self {
            db,
            secrets,
            db_path,
            web_dir,
            auth: Arc::new(auth),
            login_limiter: Arc::new(crate::auth::LoginLimiter::default()),
            events: Arc::new(EventBus::new(heartbeat)),
            imports: Arc::new(crate::importer::Registry::new(vec![Arc::new(
                crate::importer::github::GitHub,
            )])),
            openchamber: None,
            dispatch_in_flight: Arc::new(Mutex::new(HashSet::new())),
            health: Arc::new(Mutex::new(None)),
            health_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Override provider adapters, primarily for tests.
    pub fn with_import_providers(
        mut self,
        providers: Vec<Arc<dyn crate::importer::Provider>>,
    ) -> Self {
        self.imports = Arc::new(crate::importer::Registry::new(providers));
        self
    }

    /// Attach a configured OpenChamber adapter. `kanso serve` starts the
    /// background poller; tests can drive poll cycles deterministically.
    pub fn with_openchamber(
        mut self,
        openchamber: crate::adapter::openchamber::OpenChamber,
    ) -> Self {
        self.openchamber = Some(Arc::new(openchamber));
        self
    }

    /// Claim the dispatch slot for one card. The HTTP call can take minutes,
    /// so this is an in-memory reservation rather than a database lock.
    pub(crate) fn reserve_dispatch(&self, task_id: &str) -> bool {
        self.dispatch_in_flight
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(task_id.to_owned())
    }

    pub(crate) fn release_dispatch(&self, task_id: &str) {
        self.dispatch_in_flight
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(task_id);
    }

    /// Publish an event after a transaction has committed.
    pub fn publish(&self, kind: &str, data: serde_json::Value) {
        self.events.publish(kind, data);
    }

    fn cached_health(&self, now: Instant) -> Option<bool> {
        let cache = self
            .health
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cache.and_then(|entry| {
            (now.duration_since(entry.checked_at) < HEALTH_CACHE_TTL).then_some(entry.healthy)
        })
    }

    fn store_health(&self, healthy: bool) {
        let mut cache = self
            .health
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *cache = Some(HealthCache {
            checked_at: Instant::now(),
            healthy,
        });
    }

    /// Refresh the cached health in a detached task.
    ///
    /// The task owns the single-flight lock, so dropping the HTTP handler (for
    /// example on client disconnect) cannot release it while `quick_check` runs
    /// or skip storing the result.
    async fn refresh_health(&self) -> bool {
        let state = self.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _guard = state.health_lock.lock().await;
            if let Some(healthy) = state.cached_health(Instant::now()) {
                let _ = sender.send(healthy);
                return;
            }

            let db_path = state.db_path.clone();
            let healthy = tokio::task::spawn_blocking(move || db::quick_check(&db_path))
                .await
                .unwrap_or(false);
            state.store_health(healthy);
            let _ = sender.send(healthy);
        });
        receiver.await.unwrap_or(false)
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .merge(crate::api::routes())
        .route("/", get(crate::web::index))
        .route("/{*path}", get(crate::web::asset))
        .with_state(state)
}

#[derive(Debug, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub version: &'static str,
    pub db: &'static str,
}

pub async fn health(State(state): State<AppState>) -> Response {
    if let Some(healthy) = state.cached_health(Instant::now()) {
        return health_response(healthy);
    }

    health_response(state.refresh_health().await)
}

fn health_response(healthy: bool) -> Response {
    if healthy {
        (
            StatusCode::OK,
            Json(Health {
                status: "ok",
                version: VERSION,
                db: "ok",
            }),
        )
            .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(Health {
                status: "error",
                version: VERSION,
                db: "error",
            }),
        )
            .into_response()
    }
}

/// Resolve on SIGINT or SIGTERM so in-flight requests can finish.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to listen for Ctrl+C");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }

    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request};
    use tower::ServiceExt;

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        serde_json::from_slice(&bytes).expect("parse response JSON")
    }

    fn migrated_state(dir: &TempDir) -> AppState {
        let database = Database::open(&dir.path().join("kanso.db")).expect("open database");
        let auth = AuthConfig::load("secret", None, dir.path()).expect("auth config");
        AppState::new(database, dir.path().join("web"), auth)
    }

    fn state_with_health_path(dir: &TempDir, health_path: PathBuf) -> AppState {
        let mut state = migrated_state(dir);
        state.db_path = health_path;
        state
    }

    #[tokio::test]
    async fn health_reports_ok_for_a_valid_database() {
        let dir = TempDir::new();
        let response = health(State(migrated_state(&dir))).await;
        assert_eq!(response.status(), StatusCode::OK);

        let body = body_json(response).await;
        assert_eq!(body["status"], "ok");
        assert_eq!(body["db"], "ok");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn health_reports_unavailable_without_a_database() {
        let dir = TempDir::new();
        let response = health(State(state_with_health_path(
            &dir,
            dir.path().join("missing.db"),
        )))
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = body_json(response).await;
        assert_eq!(body["status"], "error");
        assert_eq!(body["db"], "error");
    }

    #[tokio::test]
    async fn health_reports_unavailable_for_a_corrupt_database() {
        let dir = TempDir::new();
        let corrupt = dir.path().join("corrupt.db");
        std::fs::write(&corrupt, b"not a sqlite database").expect("write corrupt database");

        let response = health(State(state_with_health_path(&dir, corrupt))).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(response).await["db"], "error");
    }

    #[tokio::test]
    async fn health_route_is_registered_with_json_content_type() {
        let dir = TempDir::new();
        let app = router(migrated_state(&dir));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("call router");

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_type.starts_with("application/json"),
            "{content_type}"
        );
        assert_eq!(body_json(response).await["status"], "ok");
    }
}
