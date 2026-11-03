//! M5 adapter tests against a local mock OpenChamber server.
//!
//! Nothing here contacts the real instance: the mock binds `127.0.0.1:0`,
//! records every request, and can answer busy/idle/failure sequences.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::connect_info::MockConnectInfo;
use axum::extract::State;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use kanso::adapter::openchamber::OpenChamber;
use kanso::adapter::poller::Poller;
use kanso::auth::AuthConfig;
use kanso::db::Database;
use kanso::server::{router, AppState};
use serde_json::{json, Value};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse";
const ORIGIN: &str = "https://kanso.example";
const TOKEN: &str = "oc_client_test_token";
static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Mock OpenChamber

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Value,
}

#[derive(Clone)]
struct Mock {
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    create_status: Arc<AtomicU16>,
    create_body: Arc<Mutex<Value>>,
    create_delay_ms: Arc<AtomicU64>,
    auto_accept_status: Arc<AtomicU16>,
    version_status: Arc<AtomicU16>,
    list_queue: Arc<Mutex<VecDeque<(u16, Value)>>>,
}

impl Mock {
    fn new() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            create_status: Arc::new(AtomicU16::new(200)),
            create_delay_ms: Arc::new(AtomicU64::new(0)),
            create_body: Arc::new(Mutex::new(json!({
                "sessionId": "ses_abc",
                "directory": "/tmp/kanso-session",
            }))),
            auto_accept_status: Arc::new(AtomicU16::new(200)),
            version_status: Arc::new(AtomicU16::new(200)),
            list_queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    fn record(&self, method: &str, path: &str, headers: &HeaderMap, body: Value) {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(RecordedRequest {
                method: method.to_owned(),
                path: path.to_owned(),
                authorization: headers
                    .get(header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                body,
            });
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn create_requests(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|request| {
                request.method == "POST"
                    && request.body.get("action").and_then(Value::as_str) == Some("session.create")
            })
            .collect()
    }

    fn queue_list(&self, status: u16, body: Value) {
        self.list_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back((status, body));
    }

    fn set_create(&self, status: u16, body: Value) {
        self.create_status.store(status, Ordering::SeqCst);
        *self
            .create_body
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = body;
    }

    fn set_auto_accept_status(&self, status: u16) {
        self.auto_accept_status.store(status, Ordering::SeqCst);
    }

    fn set_create_delay(&self, millis: u64) {
        self.create_delay_ms.store(millis, Ordering::SeqCst);
    }

    fn list_requests(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|request| {
                request.method == "POST"
                    && request.body.get("action").and_then(Value::as_str) == Some("session.list")
            })
            .collect()
    }
}

fn response(status: u16, body: Value) -> Response {
    (
        StatusCode::from_u16(status).expect("status code"),
        Json(body),
    )
        .into_response()
}

async fn mock_version(State(mock): State<Mock>, headers: HeaderMap) -> Response {
    mock.record("GET", "/api/version", &headers, Value::Null);
    let status = mock.version_status.load(Ordering::SeqCst);
    if status == 200 {
        response(
            status,
            json!({
                "status": "ok",
                "openchamberVersion": "2.6.0",
                "runtime": "web",
                "capabilities": [],
            }),
        )
    } else {
        response(status, json!({ "error": "version probe failed" }))
    }
}

async fn mock_control(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    mock.record(
        "POST",
        "/api/openchamber/control",
        &headers,
        payload.clone(),
    );
    match payload.get("action").and_then(Value::as_str) {
        Some("session.create") => {
            let delay = mock.create_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            let status = mock.create_status.load(Ordering::SeqCst);
            let body = mock
                .create_body
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            response(status, body)
        }
        Some("session.list") => {
            let queued = mock
                .list_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop_front();
            match queued {
                Some((status, body)) => response(status, body),
                None => response(200, json!({ "sessions": [] })),
            }
        }
        _ => response(400, json!({ "error": "unknown action" })),
    }
}

async fn mock_auto_accept(
    State(mock): State<Mock>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    mock.record(
        "PUT",
        &format!("/api/permission-auto-accept/sessions/{session_id}"),
        &headers,
        payload,
    );
    let status = mock.auto_accept_status.load(Ordering::SeqCst);
    if status == 200 {
        response(200, json!({ "ok": true }))
    } else {
        response(status, json!({ "error": "auto-accept failed" }))
    }
}

async fn start_mock() -> (String, Mock) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let address = listener.local_addr().expect("mock address");
    let mock = Mock::new();
    let app = Router::new()
        .route("/api/version", get(mock_version))
        .route("/api/openchamber/control", post(mock_control))
        .route(
            "/api/permission-auto-accept/sessions/{id}",
            put(mock_auto_accept),
        )
        .with_state(mock.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), mock)
}

async fn wait_for_create(mock: &Mock) {
    for _ in 0..200 {
        if !mock.create_requests().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("dispatch never reached OpenChamber");
}

// ---------------------------------------------------------------------------
// Kanso test app

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        for _ in 0..100 {
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("kanso-m5-{}-{sequence}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create temporary directory {}: {error}", path.display()),
            }
        }
        panic!("could not create a unique temporary directory");
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct TestApp {
    _dir: TempDir,
    state: AppState,
    app: Router,
    poller: tokio::sync::Mutex<Poller>,
}

struct ApiResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl ApiResponse {
    fn code(&self) -> &str {
        self.body
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    fn message(&self) -> &str {
        self.body
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    fn session_cookie(&self) -> String {
        let cookie = self
            .headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with("kanso_session="))
            .expect("kanso_session Set-Cookie");
        cookie
            .split(';')
            .next()
            .and_then(|pair| pair.strip_prefix("kanso_session="))
            .expect("cookie value")
            .to_owned()
    }
}

impl TestApp {
    async fn new(openchamber_url: Option<&str>) -> Self {
        let dir = TempDir::new();
        let database = Database::open(&dir.path().join("kanso.db")).expect("open database");
        let auth = AuthConfig::load(PASSWORD, Some(ORIGIN), dir.path()).expect("auth config");
        let mut state = AppState::new(database, dir.path().join("web"), auth);
        if let Some(url) = openchamber_url {
            state = state.with_openchamber(OpenChamber::new(url, TOKEN));
        }
        let app = router(state.clone())
            .layer(MockConnectInfo(SocketAddr::from(([203, 0, 113, 7], 4444))));
        Self {
            _dir: dir,
            state,
            app,
            poller: tokio::sync::Mutex::new(Poller::new()),
        }
    }

    async fn request(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
        cookie: Option<&str>,
        origin: Option<&str>,
    ) -> ApiResponse {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "kanso.example");
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, format!("kanso_session={cookie}"));
        }
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        let body = match body {
            Some(value) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .app
            .clone()
            .oneshot(builder.body(body).expect("build request"))
            .await
            .expect("call router");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        ApiResponse {
            status,
            headers,
            body,
        }
    }

    async fn login(&self) -> String {
        let response = self
            .request(
                Method::POST,
                "/api/auth/login",
                Some(json!({ "password": PASSWORD })),
                None,
                None,
            )
            .await;
        assert_eq!(response.status, StatusCode::NO_CONTENT);
        response.session_cookie()
    }

    async fn get(&self, uri: &str, cookie: &str) -> ApiResponse {
        self.request(Method::GET, uri, None, Some(cookie), None)
            .await
    }

    async fn post(&self, uri: &str, body: Option<Value>, cookie: &str) -> ApiResponse {
        self.request(Method::POST, uri, body, Some(cookie), Some(ORIGIN))
            .await
    }

    async fn post_bearer(&self, uri: &str, body: Option<Value>, bearer: &str) -> ApiResponse {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::HOST, "kanso.example")
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        let body = match body {
            Some(value) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .app
            .clone()
            .oneshot(builder.body(body).expect("build request"))
            .await
            .expect("call router");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        ApiResponse {
            status,
            headers,
            body,
        }
    }

    async fn patch(&self, uri: &str, body: Value, cookie: &str) -> ApiResponse {
        self.request(Method::PATCH, uri, Some(body), Some(cookie), Some(ORIGIN))
            .await
    }

    async fn create_project(&self, cookie: &str, key: &str, name: &str) -> Value {
        let response = self
            .post(
                "/api/projects",
                Some(json!({ "key": key, "name": name })),
                cookie,
            )
            .await;
        assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.body);
        response.body
    }

    async fn create_card(&self, cookie: &str, project_id: &str, title: &str) -> Value {
        let response = self
            .post(
                "/api/tasks",
                Some(json!({
                    "project_id": project_id,
                    "title": title,
                    "description": "Details\nline 2",
                })),
                cookie,
            )
            .await;
        assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.body);
        response.body
    }

    async fn detail(&self, cookie: &str, reference: &str) -> Value {
        let response = self.get(&format!("/api/tasks/{reference}"), cookie).await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
        response.body
    }

    async fn poll(&self) {
        let mut poller = self.poller.lock().await;
        poller
            .poll_once(&self.state)
            .await
            .expect("poll OpenChamber");
    }
}

// ---------------------------------------------------------------------------
// Config and integration

#[tokio::test]
async fn disabled_integration_reports_so_and_refuses_dispatch() {
    let app = TestApp::new(None).await;
    let cookie = app.login().await;

    let integration = app.get("/api/integration", &cookie).await;
    assert_eq!(integration.status, StatusCode::OK);
    assert_eq!(
        integration.body,
        json!({ "openchamber": { "enabled": false } })
    );

    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().expect("id")),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Do it")
        .await;

    let dispatch = app
        .post(
            &format!(
                "/api/tasks/{}/dispatch",
                card["reference"].as_str().expect("ref")
            ),
            None,
            &cookie,
        )
        .await;
    assert_eq!(dispatch.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(dispatch.code(), "integration_disabled");
}

#[tokio::test]
async fn integration_reports_url_reachable_and_version_without_leaking_the_token() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;

    let integration = app.get("/api/integration", &cookie).await;
    assert_eq!(integration.status, StatusCode::OK);
    assert_eq!(integration.body["openchamber"]["enabled"], true);
    assert_eq!(integration.body["openchamber"]["url"], url);
    assert_eq!(integration.body["openchamber"]["reachable"], true);
    assert_eq!(integration.body["openchamber"]["version"], "2.6.0");
    assert!(!integration.body.to_string().contains(TOKEN));

    let version_calls = mock
        .requests()
        .into_iter()
        .filter(|request| request.path == "/api/version")
        .count();
    assert_eq!(version_calls, 1);

    // The cache prevents a second settings refresh from probing again.
    let again = app.get("/api/integration", &cookie).await;
    assert_eq!(again.body["openchamber"]["reachable"], true);
    let version_calls = mock
        .requests()
        .into_iter()
        .filter(|request| request.path == "/api/version")
        .count();
    assert_eq!(version_calls, 1);
}

#[tokio::test]
async fn integration_stays_reachable_false_when_the_version_probe_fails() {
    let (url, mock) = start_mock().await;
    mock.version_status.store(500, Ordering::SeqCst);
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;

    let integration = app.get("/api/integration", &cookie).await;
    assert_eq!(integration.status, StatusCode::OK);
    assert_eq!(integration.body["openchamber"]["enabled"], true);
    assert_eq!(integration.body["openchamber"]["reachable"], false);
    assert!(integration.body["openchamber"]["version"].is_null());
}

// ---------------------------------------------------------------------------
// Dispatch

#[tokio::test]
async fn dispatch_rejects_a_project_without_a_repo_path() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let card = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Do it")
        .await;

    let response = app
        .post(
            &format!(
                "/api/tasks/{}/dispatch",
                card["reference"].as_str().expect("ref")
            ),
            None,
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.code(), "validation");
    assert!(response.message().contains("repo_path"));
    assert!(mock.create_requests().is_empty());
}

#[tokio::test]
async fn dispatch_rejects_a_claimed_card() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().expect("id")),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Do it")
        .await;
    let reference = card["reference"].as_str().expect("ref");
    app.post(&format!("/api/tasks/{reference}/claim"), None, &cookie)
        .await;

    let response = app
        .post(&format!("/api/tasks/{reference}/dispatch"), None, &cookie)
        .await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(response.code(), "already_claimed");
    assert!(mock.create_requests().is_empty());
}

#[tokio::test]
async fn dispatch_is_human_only() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().expect("id")),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Do it")
        .await;
    let reference = card["reference"].as_str().expect("ref");

    let token_response = app
        .post(
            "/api/tokens",
            Some(json!({ "name": "build", "scope": "agent" })),
            &cookie,
        )
        .await;
    assert_eq!(token_response.status, StatusCode::CREATED);
    let token = token_response.body["token"].as_str().expect("token");

    let response = app
        .post_bearer(&format!("/api/tasks/{reference}/dispatch"), None, token)
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.code(), "forbidden");
    assert!(mock.create_requests().is_empty());
}

#[tokio::test]
async fn dispatch_reports_503_when_openchamber_is_unreachable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind spare port");
    let address = listener.local_addr().expect("spare address");
    drop(listener);

    let app = TestApp::new(Some(&format!("http://{address}"))).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().expect("id")),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Do it")
        .await;
    let reference = card["reference"].as_str().expect("ref");

    let response = app
        .post(&format!("/api/tasks/{reference}/dispatch"), None, &cookie)
        .await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.code(), "integration_unreachable");
    let detail = app.detail(&cookie, reference).await;
    assert!(detail["session_id"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_does_not_overwrite_a_claim_made_during_the_upstream_call() {
    let (url, mock) = start_mock().await;
    mock.set_create_delay(500);
    let app = Arc::new(TestApp::new(Some(&url)).await);
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().expect("id")),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Do it")
        .await;
    let reference = card["reference"].as_str().expect("ref").to_owned();
    let uri = format!("/api/tasks/{reference}/dispatch");

    let dispatch_app = app.clone();
    let dispatch_cookie = cookie.clone();
    let dispatch_uri = uri.clone();
    let dispatch = tokio::spawn(async move {
        dispatch_app
            .post(&dispatch_uri, None, &dispatch_cookie)
            .await
    });
    wait_for_create(&mock).await;

    let claim = app
        .post(&format!("/api/tasks/{reference}/claim"), None, &cookie)
        .await;
    assert_eq!(claim.status, StatusCode::OK, "{:?}", claim.body);

    let response = dispatch.await.expect("dispatch task");
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(response.code(), "already_claimed");
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["claimed_by"], "ui");
    assert!(detail["session_id"].is_null());
    let kinds: Vec<&str> = detail["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"dispatch_abandoned"), "{kinds:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_does_not_overwrite_a_move_made_during_the_upstream_call() {
    let (url, mock) = start_mock().await;
    mock.set_create_delay(500);
    let app = Arc::new(TestApp::new(Some(&url)).await);
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().expect("id")),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Do it")
        .await;
    let reference = card["reference"].as_str().expect("ref").to_owned();
    let uri = format!("/api/tasks/{reference}/dispatch");

    let dispatch_app = app.clone();
    let dispatch_cookie = cookie.clone();
    let dispatch_uri = uri.clone();
    let dispatch = tokio::spawn(async move {
        dispatch_app
            .post(&dispatch_uri, None, &dispatch_cookie)
            .await
    });
    wait_for_create(&mock).await;

    let moved = app
        .post(
            &format!("/api/tasks/{reference}/move"),
            Some(json!({ "status": "blocker", "reason": "human moved it" })),
            &cookie,
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);

    let response = dispatch.await.expect("dispatch task");
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(response.code(), "conflict");
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["status"], "blocker");
    assert_eq!(detail["blocked_reason"], "human moved it");
    assert!(detail["session_id"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_does_not_overwrite_a_manual_session_link_made_during_the_call() {
    let (url, mock) = start_mock().await;
    mock.set_create_delay(500);
    let app = Arc::new(TestApp::new(Some(&url)).await);
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().expect("id")),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Do it")
        .await;
    let reference = card["reference"].as_str().expect("ref").to_owned();
    let uri = format!("/api/tasks/{reference}/dispatch");

    let dispatch_app = app.clone();
    let dispatch_cookie = cookie.clone();
    let dispatch_uri = uri.clone();
    let dispatch = tokio::spawn(async move {
        dispatch_app
            .post(&dispatch_uri, None, &dispatch_cookie)
            .await
    });
    wait_for_create(&mock).await;

    let linked = app
        .post(
            &format!("/api/tasks/{reference}/session"),
            Some(json!({
                "session_id": "ses_manual",
                "directory": "/tmp/manual",
                "status": "running",
            })),
            &cookie,
        )
        .await;
    assert_eq!(linked.status, StatusCode::OK, "{:?}", linked.body);

    let response = dispatch.await.expect("dispatch task");
    assert_eq!(response.status, StatusCode::CONFLICT);
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["session_id"], "ses_manual");
    assert_eq!(detail["session_directory"], "/tmp/manual");
}

#[tokio::test]
async fn dispatch_from_blocker_or_done_clears_the_old_column_metadata() {
    let (url, _mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().expect("id")),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;

    let blocked = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Blocked")
        .await;
    let blocked_ref = blocked["reference"].as_str().expect("ref");
    app.post(
        &format!("/api/tasks/{blocked_ref}/move"),
        Some(json!({ "status": "blocker", "reason": "waiting" })),
        &cookie,
    )
    .await;
    let response = app
        .post(&format!("/api/tasks/{blocked_ref}/dispatch"), None, &cookie)
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    assert_eq!(response.body["status"], "in_progress");
    assert!(response.body["blocked_from"].is_null());
    assert!(response.body["blocked_reason"].is_null());

    let done = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Done")
        .await;
    let done_ref = done["reference"].as_str().expect("ref");
    let moved = app
        .post(
            &format!("/api/tasks/{done_ref}/move"),
            Some(json!({ "status": "done" })),
            &cookie,
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);
    let response = app
        .post(&format!("/api/tasks/{done_ref}/dispatch"), None, &cookie)
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    assert_eq!(response.body["status"], "in_progress");
    assert!(response.body["done_at"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_dispatch_creates_only_one_session() {
    let (url, mock) = start_mock().await;
    mock.set_create_delay(200);
    let app = Arc::new(TestApp::new(Some(&url)).await);
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().expect("id")),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Do it")
        .await;
    let reference = card["reference"].as_str().expect("ref");
    let uri = format!("/api/tasks/{reference}/dispatch");

    let (first, second) =
        tokio::join!(app.post(&uri, None, &cookie), app.post(&uri, None, &cookie));
    let mut statuses = [first.status, second.status];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::CONFLICT],
        "{:?} {:?}",
        first.body,
        second.body
    );
    assert_eq!(mock.create_requests().len(), 1);
}

#[tokio::test]
async fn dispatch_rejects_a_card_that_is_already_running() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().expect("id")),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Do it")
        .await;
    let reference = card["reference"].as_str().expect("ref");

    let first = app
        .post(&format!("/api/tasks/{reference}/dispatch"), None, &cookie)
        .await;
    assert_eq!(first.status, StatusCode::OK, "{:?}", first.body);

    let second = app
        .post(&format!("/api/tasks/{reference}/dispatch"), None, &cookie)
        .await;
    assert_eq!(second.status, StatusCode::CONFLICT);
    assert_eq!(second.code(), "already_dispatched");
    assert_eq!(mock.create_requests().len(), 1);
}

#[tokio::test]
async fn dispatch_sends_the_exact_payload_stores_the_session_and_enables_auto_accept() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let project_id = project["id"].as_str().expect("id");
    let prompt = "Card {{ref}}: {{title}}\nProject {{project}}\n{{description}}\nUnknown {{nope}}";
    let patched = app
        .patch(
            &format!("/api/projects/{project_id}"),
            json!({
                "repo_path": "/home/mathieu/code/alpha",
                "agent": "plan",
                "model": "openai/gpt-5",
                "base_branch": "develop",
                "worktree": true,
                "prompt_template": prompt,
            }),
            &cookie,
        )
        .await;
    assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
    let card = app
        .create_card(&cookie, project_id, "Fix the flux capacitor")
        .await;
    let reference = card["reference"].as_str().expect("ref");
    assert_eq!(reference, "ALP-1");

    let response = app
        .post(&format!("/api/tasks/{reference}/dispatch"), None, &cookie)
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    assert_eq!(response.body["session_id"], "ses_abc");
    assert_eq!(response.body["session_directory"], "/tmp/kanso-session");
    assert_eq!(response.body["session_status"], "running");
    assert_eq!(response.body["claimed_by"], "openchamber");
    assert_eq!(response.body["status"], "in_progress");
    assert_eq!(response.body["attention"], false);
    assert!(!response.body.to_string().contains(TOKEN));

    let creates = mock.create_requests();
    assert_eq!(creates.len(), 1);
    let request = &creates[0];
    let bearer = format!("Bearer {TOKEN}");
    assert_eq!(request.path, "/api/openchamber/control");
    assert_eq!(request.authorization.as_deref(), Some(bearer.as_str()));
    assert_eq!(
        request.body,
        json!({
            "action": "session.create",
            "input": {
                "directory": "/home/mathieu/code/alpha",
                "title": "ALP-1 — Fix the flux capacitor",
                "prompt": "Card ALP-1: Fix the flux capacitor\nProject Alpha\nDetails\nline 2\nUnknown {{nope}}",
                "agent": "plan",
                "model": "openai/gpt-5",
                "worktree": "kanso/ALP-1-fix-the-flux-capacitor",
                "branch": "kanso/ALP-1-fix-the-flux-capacitor",
                "startRef": "develop",
                "wait": false
            }
        })
    );

    let auto_accept = mock
        .requests()
        .into_iter()
        .find(|request| request.method == "PUT")
        .expect("auto-accept request");
    assert_eq!(
        auto_accept.path,
        "/api/permission-auto-accept/sessions/ses_abc"
    );
    assert_eq!(auto_accept.authorization.as_deref(), Some(bearer.as_str()));
    assert_eq!(
        auto_accept.body,
        json!({ "mode": "auto", "directory": "/tmp/kanso-session" })
    );

    let detail = app.detail(&cookie, reference).await;
    assert_eq!(detail["session_id"], "ses_abc");
    assert_eq!(detail["claimed_by"], "openchamber");
    assert_eq!(detail["status"], "in_progress");
    let kinds: Vec<&str> = detail["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"dispatched"), "{kinds:?}");
    assert!(
        !kinds.contains(&"permission_auto_accept_failed"),
        "{kinds:?}"
    );
}

#[tokio::test]
async fn dispatch_surfaces_an_upstream_rejection_as_a_bad_gateway() {
    let (url, mock) = start_mock().await;
    mock.set_create(400, json!({ "error": "worktree provisioning failed" }));
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().expect("id")),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Do it")
        .await;
    let reference = card["reference"].as_str().expect("ref");

    let response = app
        .post(&format!("/api/tasks/{reference}/dispatch"), None, &cookie)
        .await;
    assert_eq!(response.status, StatusCode::BAD_GATEWAY);
    assert_eq!(response.code(), "openchamber_error");
    assert!(response.message().contains("worktree provisioning failed"));

    let detail = app.detail(&cookie, reference).await;
    assert!(detail["session_id"].is_null());
    assert!(detail["claimed_by"].is_null());
}

#[tokio::test]
async fn auto_accept_failure_keeps_the_dispatch_and_is_recorded() {
    let (url, mock) = start_mock().await;
    mock.set_auto_accept_status(500);
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().expect("id")),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Do it")
        .await;
    let reference = card["reference"].as_str().expect("ref");

    let response = app
        .post(&format!("/api/tasks/{reference}/dispatch"), None, &cookie)
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    assert_eq!(response.body["session_id"], "ses_abc");
    assert_eq!(response.body["claimed_by"], "openchamber");
    let kinds: Vec<&str> = response.body["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"dispatched"), "{kinds:?}");
    assert!(
        kinds.contains(&"permission_auto_accept_failed"),
        "{kinds:?}"
    );
}

// ---------------------------------------------------------------------------
// Poller

async fn linked_card(app: &TestApp, cookie: &str, key: &str) -> (String, String) {
    let project = app.create_project(cookie, key, "Alpha").await;
    let card = app
        .create_card(cookie, project["id"].as_str().expect("id"), "Do it")
        .await;
    let reference = card["reference"].as_str().expect("ref").to_owned();
    let response = app
        .post(
            &format!("/api/tasks/{reference}/session"),
            Some(json!({
                "session_id": "ses_poll",
                "directory": "/tmp/kanso-poll",
                "status": "running",
            })),
            cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    (project["id"].as_str().expect("id").to_owned(), reference)
}

#[tokio::test]
async fn poller_moves_a_busy_then_idle_session_to_review() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let (_, reference) = linked_card(&app, &cookie, "ALP").await;
    let moved = app
        .post(
            &format!("/api/tasks/{reference}/move"),
            Some(json!({ "status": "in_progress" })),
            &cookie,
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);

    mock.queue_list(
        200,
        json!({
            "sessions": [{
                "id": "ses_poll",
                "status": { "type": "busy" },
                "time": { "created": 1, "busy": 100 },
            }]
        }),
    );
    app.poll().await;
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["session_status"], "running");
    assert_eq!(detail["status"], "in_progress");

    mock.queue_list(
        200,
        json!({
            "sessions": [{
                "id": "ses_poll",
                "status": { "type": "idle" },
                "outcome": "succeeded",
                "time": { "created": 1, "busy": 100, "idle": 200 },
            }]
        }),
    );
    app.poll().await;
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["session_status"], "idle");
    assert_eq!(detail["status"], "in_review");
    let kinds: Vec<&str> = detail["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"auto_moved"), "{kinds:?}");

    // Idle is terminal: another cycle does not touch the card again.
    let activity_count = detail["activity"].as_array().expect("activity").len();
    app.poll().await;
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(
        detail["activity"].as_array().expect("activity").len(),
        activity_count
    );
}

#[tokio::test]
async fn poller_marks_a_non_success_outcome_failed_and_asks_for_attention() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let (_, reference) = linked_card(&app, &cookie, "ALP").await;

    mock.queue_list(
        200,
        json!({
            "sessions": [{
                "id": "ses_poll",
                "status": { "type": "idle" },
                "outcome": "failed",
                "time": { "created": 1, "busy": 100, "idle": 200 },
            }]
        }),
    );
    app.poll().await;
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["session_status"], "failed");
    assert_eq!(detail["attention"], true);
    assert_eq!(detail["status"], "backlog");
    let kinds: Vec<&str> = detail["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"session_failed"), "{kinds:?}");
}

#[tokio::test]
async fn three_consecutive_poll_failures_mark_offline_and_attention() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let (_, reference) = linked_card(&app, &cookie, "ALP").await;

    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["session_status"], "running");

    mock.queue_list(500, json!({ "error": "boom" }));
    mock.queue_list(500, json!({ "error": "boom" }));
    app.poll().await;
    app.poll().await;
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["session_status"], "running");
    assert_eq!(detail["attention"], false);

    mock.queue_list(500, json!({ "error": "boom" }));
    app.poll().await;
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["session_status"], "offline");
    assert_eq!(detail["attention"], true);
    assert_eq!(detail["status"], "backlog");
    let kinds: Vec<&str> = detail["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"session_offline"), "{kinds:?}");
}

#[tokio::test]
async fn three_missing_sessions_mark_offline_and_attention() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let (_, reference) = linked_card(&app, &cookie, "ALP").await;

    for _ in 0..2 {
        mock.queue_list(200, json!({ "sessions": [] }));
        app.poll().await;
    }
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["session_status"], "running");
    assert_eq!(detail["attention"], false);

    mock.queue_list(200, json!({ "sessions": [] }));
    app.poll().await;
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["session_status"], "offline");
    assert_eq!(detail["attention"], true);
    let kinds: Vec<&str> = detail["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"session_offline"), "{kinds:?}");
}

#[tokio::test]
async fn poller_never_auto_moves_a_done_card() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let (_, reference) = linked_card(&app, &cookie, "ALP").await;

    let moved = app
        .post(
            &format!("/api/tasks/{reference}/move"),
            Some(json!({ "status": "done" })),
            &cookie,
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);

    mock.queue_list(
        200,
        json!({
            "sessions": [{
                "id": "ses_poll",
                "status": { "type": "idle" },
                "outcome": "succeeded",
                "time": { "created": 1, "busy": 100, "idle": 200 },
            }]
        }),
    );
    app.poll().await;
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["status"], "done");
    assert_eq!(detail["session_status"], "idle");
}

#[tokio::test]
async fn poller_never_moves_a_card_the_human_put_in_blocker() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let (_, reference) = linked_card(&app, &cookie, "ALP").await;

    let moved = app
        .post(
            &format!("/api/tasks/{reference}/move"),
            Some(json!({ "status": "blocker", "reason": "waiting for a human" })),
            &cookie,
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);

    mock.queue_list(
        200,
        json!({
            "sessions": [{
                "id": "ses_poll",
                "status": { "type": "idle" },
                "outcome": "succeeded",
                "time": { "created": 1, "busy": 100, "idle": 200 },
            }]
        }),
    );
    app.poll().await;
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["status"], "blocker");
    assert_eq!(detail["session_status"], "idle");
    assert_eq!(detail["blocked_reason"], "waiting for a human");
}

#[tokio::test]
async fn poller_calls_session_list_once_per_directory() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let project_id = project["id"].as_str().expect("id");
    let first = app.create_card(&cookie, project_id, "First").await;
    let second = app.create_card(&cookie, project_id, "Second").await;
    for card in [&first, &second] {
        let reference = card["reference"].as_str().expect("ref");
        let response = app
            .post(
                &format!("/api/tasks/{reference}/session"),
                Some(json!({
                    "session_id": format!("ses_{reference}"),
                    "directory": "/tmp/kanso-shared",
                    "status": "running",
                })),
                &cookie,
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    }

    mock.queue_list(
        200,
        json!({
            "sessions": [
                {"id": "ses_ALP-1", "status": {"type": "busy"}, "time": {"busy": 10}},
                {"id": "ses_ALP-2", "status": {"type": "busy"}, "time": {"busy": 10}}
            ]
        }),
    );
    app.poll().await;
    assert_eq!(mock.list_requests().len(), 1);
    for card in [&first, &second] {
        let detail = app
            .detail(&cookie, card["reference"].as_str().expect("ref"))
            .await;
        assert_eq!(detail["session_status"], "running");
    }
}
