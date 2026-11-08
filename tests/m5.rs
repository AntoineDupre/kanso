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
use axum::http::request::Builder;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use kanso::adapter::openchamber::{
    render_prompt, OpenChamber, PromptValues, DEFAULT_PROMPT_TEMPLATE,
};
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
    send_status: Arc<AtomicU16>,
    send_body: Arc<Mutex<Value>>,
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
            send_status: Arc::new(AtomicU16::new(200)),
            send_body: Arc::new(Mutex::new(json!({ "promptDispatched": true }))),
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

    fn set_send(&self, status: u16, body: Value) {
        self.send_status.store(status, Ordering::SeqCst);
        *self
            .send_body
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = body;
    }

    fn send_requests(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|request| {
                request.method == "POST"
                    && request.body.get("action").and_then(Value::as_str) == Some("session.send")
            })
            .collect()
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
        Some("session.send") => {
            let status = mock.send_status.load(Ordering::SeqCst);
            let body = mock
                .send_body
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            response(status, body)
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

async fn mock_pairing(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    mock.record("POST", "/api/client-auth/pairing/redeem", &headers, payload);
    response(200, json!({"clientToken":"oc_client_paired"}))
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
        .route("/api/client-auth/pairing/redeem", post(mock_pairing))
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

    async fn send(&self, builder: Builder, body: Body) -> ApiResponse {
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
        self.send(builder, body).await
    }

    /// A POST with a raw body and an explicit (or absent) content type, for
    /// exercising the optional-body extractor.
    async fn post_raw(
        &self,
        uri: &str,
        body: &str,
        content_type: Option<&str>,
        cookie: &str,
    ) -> ApiResponse {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::HOST, "kanso.example")
            .header(header::COOKIE, format!("kanso_session={cookie}"))
            .header(header::ORIGIN, ORIGIN);
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        self.send(builder, Body::from(body.to_owned())).await
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

    /// A card with a linked session, moved to blocker with a question.
    async fn blocked_card(&self, cookie: &str, key: &str, reason: &str) -> String {
        let project = self.create_project(cookie, key, "Alpha").await;
        let card = self
            .create_card(cookie, project["id"].as_str().expect("id"), "Do it")
            .await;
        let reference = card["reference"].as_str().expect("ref").to_owned();
        let linked = self
            .post(
                &format!("/api/tasks/{reference}/session"),
                Some(json!({
                    "session_id": "ses_reply",
                    "directory": "/tmp/kanso-session",
                    "status": "running",
                })),
                cookie,
            )
            .await;
        assert_eq!(linked.status, StatusCode::OK, "{:?}", linked.body);
        let moved = self
            .post(
                &format!("/api/tasks/{reference}/move"),
                Some(json!({ "status": "blocker", "reason": reason })),
                cookie,
            )
            .await;
        assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);
        reference
    }
}

// ---------------------------------------------------------------------------
// Config and integration

async fn secret_fixture(url: &str) -> (TestApp, String, String, String, Value) {
    let app = TestApp::new(Some(url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "HND", "Handoffs").await;
    let card = app
        .create_card(&cookie, project["id"].as_str().unwrap(), "Use a credential")
        .await;
    let reference = card["reference"].as_str().unwrap().to_owned();
    let linked = app
        .post(
            &format!("/api/tasks/{reference}/session"),
            Some(json!({"session_id":"ses_handoff","directory":"/work","status":"waiting"})),
            &cookie,
        )
        .await;
    assert_eq!(linked.status, StatusCode::OK);
    let moved = app
        .post(
            &format!("/api/tasks/{reference}/move"),
            Some(json!({"status":"in_progress"})),
            &cookie,
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK);
    let token = app
        .post(
            "/api/tokens",
            Some(json!({"name":"handoff-agent","scope":"agent"})),
            &cookie,
        )
        .await
        .body["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let created = app
        .post_bearer(
            &format!("/api/tasks/{reference}/secrets"),
            Some(json!({"label":"API credential","purpose":"Authenticate one command"})),
            &token,
        )
        .await;
    assert_eq!(created.status, StatusCode::OK, "{:?}", created.body);
    (app, cookie, reference, token, created.body)
}

#[tokio::test]
async fn secret_notification_retry_is_reference_only_and_does_not_extend_expiry() {
    let (url, mock) = start_mock().await;
    let (app, cookie, reference, agent, created) = secret_fixture(&url).await;
    let id = created["id"].as_str().unwrap();
    let capability = created["capability"].as_str().unwrap();
    let value = "sentinel_credential_NEVER_PERSIST_61853";
    let mut events = app.state.events.subscribe();
    mock.set_send(
        500,
        json!({"error":format!("upstream echoed {value} {capability}")}),
    );
    let submitted = app
        .post(
            &format!("/api/secrets/{id}/submit"),
            Some(json!({"value":value})),
            &cookie,
        )
        .await;
    assert_eq!(submitted.status, StatusCode::OK);
    assert_eq!(submitted.body["delivery_status"], "failed");
    assert_eq!(submitted.headers[header::CACHE_CONTROL], "no-store");
    assert!(!submitted.body.to_string().contains(value));
    assert!(!submitted.body.to_string().contains(capability));
    let before = app.get(&format!("/api/secrets/{id}"), &cookie).await.body;
    let retry_url = format!("/api/secrets/{id}/retry");
    assert_eq!(
        app.post_bearer(&retry_url, None, &agent).await.status,
        StatusCode::FORBIDDEN
    );
    mock.set_send(200, json!({"promptDispatched":true}));
    let retried = app.post(&retry_url, None, &cookie).await;
    assert_eq!(retried.status, StatusCode::OK);
    assert_eq!(retried.body["status"], "sent");
    assert_eq!(
        app.post(&retry_url, None, &cookie).await.status,
        StatusCode::CONFLICT
    );
    let after = app.get(&format!("/api/secrets/{id}"), &cookie).await.body;
    assert_eq!(before["deadline"], after["deadline"]);
    assert_eq!(before["value_deadline"], after["value_deadline"]);
    assert_eq!(after["state"], "ready");
    let mut saw_secret_event = false;
    while let Ok(event) = events.try_recv() {
        let serialized = serde_json::to_string(&event).unwrap();
        assert!(!serialized.contains(value) && !serialized.contains(capability));
        saw_secret_event |= event.kind == "secret.updated";
    }
    assert!(saw_secret_event);
    let sends = mock.send_requests();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].body, sends[1].body);
    let prompt = sends[0].body["input"]["prompt"].as_str().unwrap();
    assert!(prompt.contains(id) && prompt.contains(&reference));
    assert!(!prompt.contains(value) && !prompt.contains(capability));
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["status"], "blocker");
    assert_eq!(detail["session_status"], "running");
    let export = app.get("/api/export.json", &cookie).await.body.to_string();
    for public in [
        detail.to_string(),
        export,
        after.to_string(),
        submitted.body.to_string(),
    ] {
        assert!(!public.contains(value));
        assert!(!public.contains(capability));
    }
    let backup = app.post("/api/backup", None, &cookie).await;
    assert_eq!(backup.status, StatusCode::OK, "{:?}", backup.body);
    for path in [
        app.state.db.path().to_path_buf(),
        PathBuf::from(format!("{}-wal", app.state.db.path().display())),
        PathBuf::from(backup.body["path"].as_str().unwrap()),
    ] {
        let bytes = std::fs::read(path).unwrap();
        for forbidden in [value, capability] {
            assert!(!bytes
                .windows(forbidden.len())
                .any(|w| w == forbidden.as_bytes()));
        }
    }
    let consumed = app
        .post_bearer(
            &format!("/api/secrets/{id}/consume"),
            Some(json!({"capability":capability})),
            &agent,
        )
        .await;
    assert_eq!(consumed.status, StatusCode::OK);
    assert_eq!(consumed.body["value"], value);
    assert_eq!(
        app.post(&retry_url, None, &cookie).await.status,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn secret_decline_replacement_cancellation_and_ordinary_question_guard() {
    let (url, mock) = start_mock().await;
    let (app, cookie, reference, agent, created) = secret_fixture(&url).await;
    let id = created["id"].as_str().unwrap();
    let declined = app
        .post(
            &format!("/api/secrets/{id}/decline"),
            Some(json!({"explanation":"Cannot grant access"})),
            &cookie,
        )
        .await;
    assert_eq!(declined.status, StatusCode::OK);
    assert_eq!(declined.body["delivery_status"], "sent");
    assert_eq!(mock.send_requests().len(), 1);
    let create_url = format!("/api/tasks/{reference}/secrets");
    let replacement = app
        .post_bearer(
            &create_url,
            Some(json!({"label":"Replacement","purpose":"Try a different credential"})),
            &agent,
        )
        .await;
    assert_eq!(replacement.status, StatusCode::OK, "{:?}", replacement.body);
    assert_ne!(replacement.body["id"], created["id"]);
    assert_ne!(replacement.body["capability"], created["capability"]);
    // A new request also prevents replay of the preceding decline message.
    assert_eq!(
        app.post(&format!("/api/secrets/{id}/retry"), None, &cookie)
            .await
            .status,
        StatusCode::CONFLICT
    );
    let next_id = replacement.body["id"].as_str().unwrap();
    assert_eq!(
        app.post_bearer(&format!("/api/secrets/{next_id}/cancel"), None, &agent)
            .await
            .status,
        StatusCode::OK
    );
    let question = app
        .post(
            &format!("/api/tasks/{reference}/move"),
            Some(json!({"status":"blocker","reason":"An unrelated ordinary question"})),
            &cookie,
        )
        .await;
    assert_eq!(question.status, StatusCode::OK);
    assert_eq!(
        app.post_bearer(
            &create_url,
            Some(json!({"label":"Not allowed","purpose":"Do not overwrite the question"})),
            &agent
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        app.detail(&cookie, &reference).await["blocked_reason"],
        "An unrelated ordinary question"
    );
}

#[tokio::test]
async fn secret_ready_value_is_removed_on_leave_archive_and_token_revocation() {
    for mutation in ["leave", "archive", "revoke"] {
        let (app, cookie, reference, agent, created) = secret_fixture("http://127.0.0.1:1").await;
        let id = created["id"].as_str().unwrap();
        let capability = created["capability"].as_str().unwrap();
        assert_eq!(
            app.post(
                &format!("/api/secrets/{id}/submit"),
                Some(json!({"value":"lifecycle-sentinel"})),
                &cookie
            )
            .await
            .status,
            StatusCode::OK
        );
        match mutation {
            "leave" => {
                assert_eq!(
                    app.post(
                        &format!("/api/tasks/{reference}/move"),
                        Some(json!({"status":"in_progress"})),
                        &cookie
                    )
                    .await
                    .status,
                    StatusCode::OK
                );
            }
            "archive" => {
                assert_eq!(
                    app.post(&format!("/api/tasks/{reference}/archive"), None, &cookie)
                        .await
                        .status,
                    StatusCode::OK
                );
            }
            "revoke" => {
                let token_id = app
                    .state
                    .db
                    .call(|db| {
                        Ok(db.query_row(
                            "SELECT id FROM tokens WHERE name='handoff-agent'",
                            [],
                            |row| row.get::<_, String>(0),
                        )?)
                    })
                    .await
                    .unwrap();
                let revoked = app
                    .request(
                        Method::DELETE,
                        &format!("/api/tokens/{token_id}"),
                        None,
                        Some(&cookie),
                        Some(ORIGIN),
                    )
                    .await;
                assert_eq!(revoked.status, StatusCode::NO_CONTENT);
            }
            _ => unreachable!(),
        }
        assert_eq!(
            app.get(&format!("/api/secrets/{id}"), &cookie).await.body["state"],
            "invalidated"
        );
        assert!(matches!(
            app.state.secrets.take(id, std::time::Instant::now()),
            Err(kanso::secret_memory::StoreError::NotFound)
        ));
        let rejected = app
            .post_bearer(
                &format!("/api/secrets/{id}/consume"),
                Some(json!({"capability":capability})),
                &agent,
            )
            .await;
        assert_eq!(
            rejected.status,
            if mutation == "revoke" {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::CONFLICT
            }
        );
    }
}

#[tokio::test]
async fn secret_handoff_authorization_atomic_consume_and_nonpersistence() {
    let app = TestApp::new(Some("http://127.0.0.1:1")).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "SEC", "Secrets").await;
    let card = app
        .create_card(&cookie, project["id"].as_str().unwrap(), "Handoff")
        .await;
    let reference = card["reference"].as_str().unwrap();
    assert_eq!(
        app.post(
            &format!("/api/tasks/{reference}/session"),
            Some(json!({"session_id":"ses_secret","directory":"/work","status":"running"})),
            &cookie
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        app.post(
            &format!("/api/tasks/{reference}/move"),
            Some(json!({"status":"in_progress"})),
            &cookie
        )
        .await
        .status,
        StatusCode::OK
    );
    let agent = app
        .post(
            "/api/tokens",
            Some(json!({"name":"secret-agent","scope":"agent"})),
            &cookie,
        )
        .await
        .body["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let other = app
        .post(
            "/api/tokens",
            Some(json!({"name":"other-agent","scope":"agent"})),
            &cookie,
        )
        .await
        .body["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let created = app
        .post_bearer(
            &format!("/api/tasks/{reference}/secrets"),
            Some(json!({"label":"API token","purpose":"connect"})),
            &agent,
        )
        .await;
    assert_eq!(created.status, StatusCode::OK, "{:?}", created.body);
    let id = created.body["id"].as_str().unwrap();
    let capability = created.body["capability"].as_str().unwrap();
    assert_eq!(app.detail(&cookie, reference).await["status"], "blocker");
    assert_eq!(
        app.post_bearer(
            &format!("/api/tasks/{reference}/secrets"),
            Some(json!({"label":"another","purpose":"test"})),
            &agent
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    let value = " secret\nsecond line\n ";
    let submit = format!("/api/secrets/{id}/submit");
    assert_eq!(
        app.post_bearer(&submit, Some(json!({"value":value})), &agent)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.request(
            Method::POST,
            &submit,
            Some(json!({"value":value})),
            Some(&cookie),
            Some("https://evil.example")
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    for invalid in [
        String::new(),
        "bad\0value".to_owned(),
        "x".repeat(16 * 1024 + 1),
    ] {
        let rejected = app
            .post(&submit, Some(json!({"value":invalid})), &cookie)
            .await;
        assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
        assert!(!rejected.body.to_string().contains("bad\\u0000value"));
    }
    assert_eq!(
        app.post(&submit, Some(json!({"value":value})), &cookie)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.post(&submit, Some(json!({"value":"replacement"})), &cookie)
            .await
            .status,
        StatusCode::CONFLICT
    );
    let consume = format!("/api/secrets/{id}/consume");
    assert_eq!(
        app.post_bearer(&consume, Some(json!({"capability":capability})), &other)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.post_bearer(&consume, Some(json!({"capability":"wrong"})), &agent)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let detail = app.detail(&cookie, reference).await.to_string();
    assert!(!detail.contains(value) && !detail.contains(capability));
    let bytes = std::fs::read(app.state.db.path()).unwrap();
    assert!(!bytes.windows(value.len()).any(|x| x == value.as_bytes()));
    assert!(!bytes
        .windows(capability.len())
        .any(|x| x == capability.as_bytes()));
    let (a, b) = tokio::join!(
        app.post_bearer(&consume, Some(json!({"capability":capability})), &agent),
        app.post_bearer(&consume, Some(json!({"capability":capability})), &agent)
    );
    let successes = [a, b]
        .into_iter()
        .filter(|r| r.status == StatusCode::OK)
        .collect::<Vec<_>>();
    assert_eq!(successes.len(), 1);
    assert_eq!(successes[0].body["value"], value);
}

#[tokio::test]
async fn secret_handoffs_invalidate_on_session_change_expiry_and_restart() {
    let app = TestApp::new(Some("http://127.0.0.1:1")).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "LIF", "Lifecycle").await;
    let card = app
        .create_card(&cookie, project["id"].as_str().unwrap(), "Handoff")
        .await;
    let reference = card["reference"].as_str().unwrap();
    let move_url = format!("/api/tasks/{reference}/move");
    let link_url = format!("/api/tasks/{reference}/session");
    let create_url = format!("/api/tasks/{reference}/secrets");
    assert_eq!(
        app.post(
            &link_url,
            Some(json!({"session_id":"ses_one","directory":"/work","status":"running"})),
            &cookie
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        app.post(&move_url, Some(json!({"status":"in_progress"})), &cookie)
            .await
            .status,
        StatusCode::OK
    );
    let agent = app
        .post(
            "/api/tokens",
            Some(json!({"name":"agent-life","scope":"agent"})),
            &cookie,
        )
        .await
        .body["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let make = || {
        app.post_bearer(
            &create_url,
            Some(json!({"label":"token","purpose":"test"})),
            &agent,
        )
    };
    let first = make().await.body;
    let first_id = first["id"].as_str().unwrap();
    assert_eq!(
        app.post(
            &format!("/api/secrets/{first_id}/submit"),
            Some(json!({"value":"first secret"})),
            &cookie
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        app.post(
            &link_url,
            Some(json!({"session_id":"ses_two","directory":"/work","status":"running"})),
            &cookie
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        app.get(&format!("/api/secrets/{first_id}"), &cookie)
            .await
            .body["state"],
        "invalidated"
    );
    assert_eq!(
        app.post_bearer(
            &format!("/api/secrets/{first_id}/consume"),
            Some(json!({"capability":first["capability"]})),
            &agent
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        app.post(&move_url, Some(json!({"status":"in_progress"})), &cookie)
            .await
            .status,
        StatusCode::OK
    );
    let second = make().await.body;
    let second_id = second["id"].as_str().unwrap().to_owned();
    assert_eq!(
        app.post(
            &format!("/api/secrets/{second_id}/submit"),
            Some(json!({"value":"second secret"})),
            &cookie
        )
        .await
        .status,
        StatusCode::OK
    );
    let expire_id = second_id.clone();
    app.state
        .db
        .call(move |db| {
            db.execute(
                "UPDATE secret_requests SET value_deadline=0 WHERE id=?1",
                [expire_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        app.get(&format!("/api/secrets/{second_id}"), &cookie)
            .await
            .body["state"],
        "expired"
    );
    let third = make().await.body;
    let third_id = third["id"].as_str().unwrap();
    assert_eq!(
        app.post(
            &format!("/api/secrets/{third_id}/submit"),
            Some(json!({"value":"third secret"})),
            &cookie
        )
        .await
        .status,
        StatusCode::OK
    );
    let restarted = Database::open(app.state.db.path()).unwrap();
    let id = third_id.to_owned();
    let status = restarted
        .call(move |db| {
            Ok(
                db.query_row("SELECT state FROM secret_requests WHERE id=?1", [id], |r| {
                    r.get::<_, String>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(status, "invalidated");
}

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

#[tokio::test]
async fn rotation_tests_authentication_and_updates_dispatch_without_restart() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    mock.queue_list(401, json!({"error": "expired"}));
    let failed = app
        .post("/api/integration/openchamber/test", None, &cookie)
        .await;
    assert_eq!(failed.body["status"], "authentication_error");
    assert_eq!(
        app.get("/api/integration", &cookie).await.body["openchamber"]["reachable"],
        true
    );

    let rotated = app
        .request(
            Method::PUT,
            "/api/integration/openchamber",
            Some(json!({"url": url, "token": "oc_client_replacement"})),
            Some(&cookie),
            Some(ORIGIN),
        )
        .await;
    assert_eq!(rotated.status, StatusCode::NO_CONTENT);
    assert!(!app
        .get("/api/integration", &cookie)
        .await
        .body
        .to_string()
        .contains("oc_client_replacement"));
    let tested = app
        .post("/api/integration/openchamber/test", None, &cookie)
        .await;
    assert_eq!(tested.body["status"], "ok");
    assert_eq!(
        mock.list_requests().last().unwrap().body["input"]["limit"],
        1
    );
    assert_eq!(
        mock.list_requests()
            .last()
            .unwrap()
            .authorization
            .as_deref(),
        Some("Bearer oc_client_replacement")
    );

    let project = app.create_project(&cookie, "ROT", "Rotation").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().unwrap()),
        json!({"repo_path": "/home/me/code/alpha"}),
        &cookie,
    )
    .await;
    let card = app
        .create_card(&cookie, project["id"].as_str().unwrap(), "Dispatch")
        .await;
    let dispatched = app
        .post(
            &format!(
                "/api/tasks/{}/dispatch",
                card["reference"].as_str().unwrap()
            ),
            None,
            &cookie,
        )
        .await;
    assert!(dispatched.status.is_success(), "{:?}", dispatched.body);
    assert_eq!(
        mock.create_requests()
            .last()
            .unwrap()
            .authorization
            .as_deref(),
        Some("Bearer oc_client_replacement")
    );
}

#[tokio::test]
async fn upstream_error_must_not_echo_the_write_only_token() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    mock.set_create(
        401,
        json!({"error": format!("rejected credential {TOKEN}")}),
    );
    let project = app.create_project(&cookie, "SEC", "Security").await;
    app.patch(
        &format!("/api/projects/{}", project["id"].as_str().unwrap()),
        json!({"repo_path":"/home/me/code/alpha"}),
        &cookie,
    )
    .await;
    let card = app
        .create_card(&cookie, project["id"].as_str().unwrap(), "Dispatch")
        .await;
    let result = app
        .post(
            &format!(
                "/api/tasks/{}/dispatch",
                card["reference"].as_str().unwrap()
            ),
            None,
            &cookie,
        )
        .await;
    assert_eq!(result.status, StatusCode::BAD_GATEWAY);
    assert!(!result.body.to_string().contains(TOKEN));
}

#[tokio::test]
async fn upstream_success_body_error_does_not_echo_token() {
    let (url, mock) = start_mock().await;
    mock.set_send(
        200,
        json!({"promptDispatched": false, "promptError": format!("rejected {TOKEN}")}),
    );
    let adapter = OpenChamber::new(&url, TOKEN);
    let result = adapter
        .send_prompt("ses_test", "/work", "hello", Duration::from_secs(2))
        .await;
    let text = format!("{result:?}");
    assert!(!text.contains(TOKEN));
    assert!(text.contains("[redacted]"));
}

#[tokio::test]
async fn connection_test_classifies_invalid_protocol_and_requires_a_human() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    mock.queue_list(200, json!({"unexpected": true}));
    let test = app
        .post("/api/integration/openchamber/test", None, &cookie)
        .await;
    assert_eq!(test.body["status"], "protocol_error");
    let agent = app
        .post(
            "/api/tokens",
            Some(json!({"name":"settings-agent","scope":"agent"})),
            &cookie,
        )
        .await;
    let bearer = agent.body["token"].as_str().expect("agent token");
    assert_eq!(
        app.post_bearer("/api/integration/openchamber/test", None, bearer)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let response = app
        .send(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/integration/openchamber")
                .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
                .header(header::CONTENT_TYPE, "application/json"),
            Body::from(json!({"url":url,"token":"bad"}).to_string()),
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn connection_test_reports_transport_failures_without_leaking_token() {
    let app = TestApp::new(Some("http://127.0.0.1:1")).await;
    let cookie = app.login().await;
    let result = app
        .post("/api/integration/openchamber/test", None, &cookie)
        .await;
    assert_eq!(result.body["status"], "transport_error");
    assert!(!result.body.to_string().contains(TOKEN));
}

#[tokio::test]
async fn settings_reject_invalid_urls_and_enforce_csrf() {
    let app = TestApp::new(None).await;
    let cookie = app.login().await;
    for url in [
        "https://name:pass@example.com",
        "http://localhost/?token=secret",
        "file:///etc/passwd",
    ] {
        let response = app
            .request(
                Method::PUT,
                "/api/integration/openchamber",
                Some(json!({"url":url,"token":"oc_client_test"})),
                Some(&cookie),
                Some(ORIGIN),
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }
    let response = app
        .request(
            Method::PUT,
            "/api/integration/openchamber",
            Some(json!({"url":"http://localhost:1234", "token":"oc_client_test"})),
            Some(&cookie),
            Some("https://attacker.example"),
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert!(app.state.openchamber().is_none());
}

#[tokio::test]
async fn changing_url_requires_a_new_token_and_does_not_redirect_the_old_one() {
    let (url, _) = start_mock().await;
    let (other_url, other) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let unchanged = app
        .request(
            Method::PUT,
            "/api/integration/openchamber",
            Some(json!({"url":url})),
            Some(&cookie),
            Some(ORIGIN),
        )
        .await;
    assert_eq!(unchanged.status, StatusCode::NO_CONTENT);
    assert!(!app._dir.path().join("openchamber-connection.json").exists());
    let changed = app
        .request(
            Method::PUT,
            "/api/integration/openchamber",
            Some(json!({"url":other_url})),
            Some(&cookie),
            Some(ORIGIN),
        )
        .await;
    assert_eq!(changed.status, StatusCode::BAD_REQUEST);
    assert_eq!(app.state.openchamber().unwrap().url(), url);
    assert!(other.requests().is_empty());
}

#[tokio::test]
async fn pairing_regeneration_redeems_and_uses_new_token() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let encoded = URL_SAFE_NO_PAD.encode(
        json!({"pairingId":"pair_test", "secret":"pairing_secret", "candidates":[]}).to_string(),
    );
    let link = format!("openchamber://connect?v=2&p={encoded}");
    let result = app
        .post(
            "/api/integration/openchamber/regenerate",
            Some(json!({"link":link})),
            &cookie,
        )
        .await;
    assert_eq!(result.status, StatusCode::NO_CONTENT, "{:?}", result.body);
    let request = mock
        .requests()
        .into_iter()
        .find(|r| r.path == "/api/client-auth/pairing/redeem")
        .expect("redeem request");
    assert_eq!(request.body["pairingId"], "pair_test");
    assert_eq!(
        app.post("/api/integration/openchamber/test", None, &cookie)
            .await
            .body["status"],
        "ok"
    );
    assert_eq!(
        mock.list_requests()
            .last()
            .unwrap()
            .authorization
            .as_deref(),
        Some("Bearer oc_client_paired")
    );
    assert!(!app
        .get("/api/integration", &cookie)
        .await
        .body
        .to_string()
        .contains("oc_client_paired"));
}

#[tokio::test]
async fn invalid_pairing_does_not_replace_existing_token() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let result = app
        .post(
            "/api/integration/openchamber/regenerate",
            Some(json!({"link":"not a pairing link"})),
            &cookie,
        )
        .await;
    assert_eq!(result.status, StatusCode::BAD_REQUEST);
    let _ = app
        .post("/api/integration/openchamber/test", None, &cookie)
        .await;
    assert_eq!(
        mock.list_requests()
            .last()
            .unwrap()
            .authorization
            .as_deref(),
        Some(&format!("Bearer {TOKEN}")[..])
    );
    assert!(!mock
        .requests()
        .iter()
        .any(|r| r.path == "/api/client-auth/pairing/redeem"));
}

#[tokio::test]
async fn pairing_regeneration_requires_human_and_csrf_protection() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let agent = app
        .post(
            "/api/tokens",
            Some(json!({"name":"pairing-agent","scope":"agent"})),
            &cookie,
        )
        .await;
    let bearer = agent.body["token"].as_str().unwrap();
    let body = Some(json!({"link":"example"}));
    assert_eq!(
        app.post_bearer(
            "/api/integration/openchamber/regenerate",
            body.clone(),
            bearer
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.request(
            Method::POST,
            "/api/integration/openchamber/regenerate",
            body,
            Some(&cookie),
            Some("https://attacker.example")
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert!(!mock
        .requests()
        .iter()
        .any(|r| r.path == "/api/client-auth/pairing/redeem"));
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
                "prompt": format!(
                    "Card ALP-1: Fix the flux capacitor\nProject Alpha\nDetails\nline 2\nUnknown {{{{nope}}}}\n\n{}",
                    render_prompt(DEFAULT_PROMPT_TEMPLATE, &PromptValues {
                        reference: "ALP-1",
                        title: "Fix the flux capacitor",
                        project: "Alpha",
                        description: "Details\nline 2",
                    })
                ),
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
async fn dispatch_worktree_override_is_per_dispatch_and_defaults_to_project() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let project_id = project["id"].as_str().expect("id");

    for (default, override_value, expected) in [
        (true, Some(false), false),
        (false, Some(true), true),
        (true, None, true),
        (false, None, false),
    ] {
        let patched = app
            .patch(
                &format!("/api/projects/{project_id}"),
                json!({ "repo_path": "/home/me/code/alpha", "worktree": default }),
                &cookie,
            )
            .await;
        assert_eq!(patched.status, StatusCode::OK);
        let card = app.create_card(&cookie, project_id, "Do it").await;
        let reference = card["reference"].as_str().expect("ref");
        let body = override_value.map(|value| json!({ "worktree": value }));
        let response = app
            .post(&format!("/api/tasks/{reference}/dispatch"), body, &cookie)
            .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
        let creates = mock.create_requests();
        let input = &creates.last().expect("create request").body["input"];
        if expected {
            assert_eq!(input["worktree"], format!("kanso/{reference}-do-it"));
        } else {
            assert_eq!(input["worktree"], false);
        }
        let project = app
            .get(&format!("/api/projects/{project_id}"), &cookie)
            .await;
        assert_eq!(project.status, StatusCode::OK);
        assert_eq!(project.body["worktree"], default);
    }
}

#[tokio::test]
async fn dispatch_body_overrides_agent_and_model_with_variant() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let project_id = project["id"].as_str().expect("id");
    let patched = app
        .patch(
            &format!("/api/projects/{project_id}"),
            json!({
                "repo_path": "/home/me/code/alpha",
                "agent": "plan",
                "model": "openai/gpt-5#max",
            }),
            &cookie,
        )
        .await;
    assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
    let card = app.create_card(&cookie, project_id, "Do it").await;
    let reference = card["reference"].as_str().expect("ref");

    let response = app
        .post(
            &format!("/api/tasks/{reference}/dispatch"),
            Some(json!({
                "agent": "review",
                "model": "anthropic/claude-sonnet#high",
            })),
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);

    let creates = mock.create_requests();
    assert_eq!(creates.len(), 1);
    let input = &creates[0].body["input"];
    assert_eq!(input["agent"], "review");
    assert_eq!(input["model"], "anthropic/claude-sonnet");
    assert_eq!(input["variant"], "high");
}

#[tokio::test]
async fn dispatch_without_a_body_inherits_and_splits_the_project_model() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let project_id = project["id"].as_str().expect("id");
    app.patch(
        &format!("/api/projects/{project_id}"),
        json!({
            "repo_path": "/home/me/code/alpha",
            "agent": "plan",
            "model": "opencode-go/deepseek-v4.1-flash#max",
        }),
        &cookie,
    )
    .await;
    let card = app.create_card(&cookie, project_id, "Do it").await;
    let reference = card["reference"].as_str().expect("ref");

    let response = app
        .post(&format!("/api/tasks/{reference}/dispatch"), None, &cookie)
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);

    let creates = mock.create_requests();
    assert_eq!(creates.len(), 1);
    let input = &creates[0].body["input"];
    assert_eq!(input["agent"], "plan");
    assert_eq!(input["model"], "opencode-go/deepseek-v4.1-flash");
    assert_eq!(input["variant"], "max");
}

#[tokio::test]
async fn dispatch_empty_overrides_inherit_the_project_settings() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let project_id = project["id"].as_str().expect("id");
    app.patch(
        &format!("/api/projects/{project_id}"),
        json!({
            "repo_path": "/home/me/code/alpha",
            "agent": "plan",
            "model": "openai/gpt-5#max",
        }),
        &cookie,
    )
    .await;
    let card = app.create_card(&cookie, project_id, "Do it").await;
    let reference = card["reference"].as_str().expect("ref");

    let response = app
        .post(
            &format!("/api/tasks/{reference}/dispatch"),
            Some(json!({ "agent": "  ", "model": "" })),
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);

    let creates = mock.create_requests();
    assert_eq!(creates.len(), 1);
    let input = &creates[0].body["input"];
    assert_eq!(input["agent"], "plan");
    assert_eq!(input["model"], "openai/gpt-5");
    assert_eq!(input["variant"], "max");
}

#[tokio::test]
async fn dispatch_rejects_an_invalid_supplied_model() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let project_id = project["id"].as_str().expect("id");
    app.patch(
        &format!("/api/projects/{project_id}"),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app.create_card(&cookie, project_id, "Do it").await;
    let reference = card["reference"].as_str().expect("ref");

    let response = app
        .post(
            &format!("/api/tasks/{reference}/dispatch"),
            Some(json!({ "model": "not-a-model" })),
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response.code(), "validation");
    assert!(response.message().contains("invalid model"));
    assert!(mock.create_requests().is_empty());

    let detail = app.detail(&cookie, reference).await;
    assert!(detail["session_id"].is_null());
    assert!(detail["claimed_by"].is_null());
}

#[tokio::test]
async fn dispatch_rejects_an_invalid_inherited_project_model() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let project_id = project["id"].as_str().expect("id").to_owned();
    app.patch(
        &format!("/api/projects/{project_id}"),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app.create_card(&cookie, &project_id, "Do it").await;
    let reference = card["reference"].as_str().expect("ref");

    // Simulate a row written before model validation existed.
    let model_project_id = project_id.clone();
    app.state
        .db
        .call(move |connection| {
            connection.execute(
                "UPDATE projects SET model = 'not-a-model' WHERE id = ?1",
                [&model_project_id],
            )?;
            Ok(())
        })
        .await
        .expect("set invalid project model");

    let response = app
        .post(&format!("/api/tasks/{reference}/dispatch"), None, &cookie)
        .await;
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response.code(), "validation");
    assert!(response.message().contains("invalid model"));
    assert!(mock.create_requests().is_empty());
}

#[tokio::test]
async fn project_model_syntax_is_validated_on_create_and_update() {
    let (url, _mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;

    let invalid_create = app
        .post(
            "/api/projects",
            Some(json!({ "key": "BAD", "name": "Bad", "model": "not-a-model" })),
            &cookie,
        )
        .await;
    assert_eq!(invalid_create.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(invalid_create.code(), "validation");
    assert!(invalid_create.message().contains("invalid model"));

    let created = app
        .post(
            "/api/projects",
            Some(json!({
                "key": "ALP",
                "name": "Alpha",
                "model": " openai/gpt-5#max ",
            })),
            &cookie,
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    assert_eq!(created.body["model"], "openai/gpt-5#max");
    let project_id = created.body["id"].as_str().expect("id").to_owned();

    let invalid_patch = app
        .patch(
            &format!("/api/projects/{project_id}"),
            json!({ "model": "provider/" }),
            &cookie,
        )
        .await;
    assert_eq!(invalid_patch.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(invalid_patch.code(), "validation");
    assert!(invalid_patch.message().contains("invalid model"));

    let unchanged = app
        .get(&format!("/api/projects/{project_id}"), &cookie)
        .await;
    assert_eq!(unchanged.body["model"], "openai/gpt-5#max");

    let cleared = app
        .patch(
            &format!("/api/projects/{project_id}"),
            json!({ "model": "   " }),
            &cookie,
        )
        .await;
    assert_eq!(cleared.status, StatusCode::OK, "{:?}", cleared.body);
    assert!(cleared.body["model"].is_null());
}

#[tokio::test]
async fn dispatch_rejects_unknown_body_fields() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let project_id = project["id"].as_str().expect("id");
    app.patch(
        &format!("/api/projects/{project_id}"),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app.create_card(&cookie, project_id, "Do it").await;
    let reference = card["reference"].as_str().expect("ref");

    let response = app
        .post(
            &format!("/api/tasks/{reference}/dispatch"),
            Some(json!({ "thinking": "max" })),
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.code(), "validation");
    assert!(mock.create_requests().is_empty());
}

#[tokio::test]
async fn dispatch_accepts_a_bodyless_request_with_a_json_content_type() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let project_id = project["id"].as_str().expect("id");
    app.patch(
        &format!("/api/projects/{project_id}"),
        json!({
            "repo_path": "/home/me/code/alpha",
            "agent": "plan",
            "model": "openai/gpt-5#max",
        }),
        &cookie,
    )
    .await;
    let card = app.create_card(&cookie, project_id, "Do it").await;
    let reference = card["reference"].as_str().expect("ref");

    let response = app
        .post_raw(
            &format!("/api/tasks/{reference}/dispatch"),
            "",
            Some("application/json"),
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);

    let creates = mock.create_requests();
    assert_eq!(creates.len(), 1);
    let input = &creates[0].body["input"];
    assert_eq!(input["agent"], "plan");
    assert_eq!(input["model"], "openai/gpt-5");
    assert_eq!(input["variant"], "max");
}

#[tokio::test]
async fn dispatch_rejects_a_non_empty_body_without_a_json_content_type() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let project_id = project["id"].as_str().expect("id");
    app.patch(
        &format!("/api/projects/{project_id}"),
        json!({ "repo_path": "/home/me/code/alpha" }),
        &cookie,
    )
    .await;
    let card = app.create_card(&cookie, project_id, "Do it").await;
    let reference = card["reference"].as_str().expect("ref");
    let uri = format!("/api/tasks/{reference}/dispatch");

    for content_type in [None, Some("text/plain")] {
        let response = app
            .post_raw(&uri, r#"{"agent":"review"}"#, content_type, &cookie)
            .await;
        assert_eq!(
            response.status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{content_type:?}: {:?}",
            response.body
        );
        assert_eq!(response.code(), "validation");
    }
    assert!(mock.create_requests().is_empty());
}

#[tokio::test]
async fn dispatch_accepts_an_openrouter_model_with_nested_slashes() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let project_id = project["id"].as_str().expect("id");
    let patched = app
        .patch(
            &format!("/api/projects/{project_id}"),
            json!({
                "repo_path": "/home/me/code/alpha",
                "model": "openrouter/anthropic/claude-sonnet#high",
            }),
            &cookie,
        )
        .await;
    assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
    let card = app.create_card(&cookie, project_id, "Do it").await;
    let reference = card["reference"].as_str().expect("ref");

    let response = app
        .post(&format!("/api/tasks/{reference}/dispatch"), None, &cookie)
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);

    let creates = mock.create_requests();
    assert_eq!(creates.len(), 1);
    let input = &creates[0].body["input"];
    assert_eq!(input["model"], "openrouter/anthropic/claude-sonnet");
    assert_eq!(input["variant"], "high");
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
// Reply dialog

#[tokio::test]
async fn reply_stores_a_comment_and_forwards_the_wrapper_to_the_session() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let reference = app
        .blocked_card(&cookie, "ALP", "Use main or develop?")
        .await;
    let task_id = app.detail(&cookie, &reference).await["id"]
        .as_str()
        .expect("id")
        .to_owned();
    app.state
        .db
        .call(move |connection| {
            connection.execute(
                "UPDATE tasks SET attention = 1, session_status = 'waiting' WHERE id = ?1",
                [&task_id],
            )?;
            Ok(())
        })
        .await
        .expect("seed attention");

    let response = app
        .post(
            &format!("/api/tasks/{reference}/reply"),
            Some(json!({ "body": "main" })),
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.body);
    let comment_id = response.body["comment"]["id"]
        .as_str()
        .expect("comment id")
        .to_owned();
    assert_eq!(response.body["comment"]["author"], "ui");
    assert_eq!(response.body["comment"]["body"], "main");
    assert_eq!(response.body["comment"]["delivery_status"], "sent");
    assert!(response.body["comment"]["delivery_error"].is_null());
    assert_eq!(response.body["delivery"]["status"], "sent");
    assert_eq!(response.body["delivery"]["session_id"], "ses_reply");
    assert!(response.body["delivery"]["error"].is_null());

    let sends = mock.send_requests();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].path, "/api/openchamber/control");
    assert_eq!(
        sends[0].authorization.as_deref(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    let input = &sends[0].body["input"];
    assert_eq!(sends[0].body["action"], "session.send");
    assert_eq!(input["sessionId"], "ses_reply");
    assert_eq!(input["directory"], "/tmp/kanso-session");
    assert_eq!(input["wait"], false);
    assert_eq!(
        input["prompt"],
        format!(
            "Human reply on {reference} — Do it (reply {comment_id}):\n> Use main or develop?\n\nmain\n\nContinue the work and move the card out of blocker when you are unblocked."
        )
    );

    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["status"], "blocker");
    assert_eq!(detail["session_status"], "running");
    assert_eq!(detail["attention"], false);
    assert_eq!(detail["blocked_reason"], "Use main or develop?");
    let comments = detail["comments"].as_array().expect("comments");
    let stored = comments
        .iter()
        .find(|comment| comment["id"] == comment_id)
        .expect("reply comment");
    assert_eq!(stored["delivery_status"], "sent");
    assert!(stored["delivery_error"].is_null());

    let activity = detail["activity"].as_array().expect("activity");
    let kinds: Vec<&str> = activity
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"commented"), "{kinds:?}");
    assert!(kinds.contains(&"reply_sent"), "{kinds:?}");
    let sent = activity
        .iter()
        .find(|entry| entry["kind"] == "reply_sent")
        .expect("reply_sent activity");
    assert_eq!(sent["data"]["comment_id"], comment_id);
    assert_eq!(sent["data"]["session_id"], "ses_reply");
    assert!(sent["data"].get("error").is_none());
    let blocked = activity
        .iter()
        .find(|entry| entry["kind"] == "blocked")
        .expect("blocked activity");
    assert_eq!(blocked["data"]["reason"], "Use main or develop?");

    let again = app
        .post(
            &format!("/api/tasks/{reference}/reply/{comment_id}"),
            None,
            &cookie,
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.code(), "already_sent");
}

#[tokio::test]
async fn a_failed_reply_is_recorded_and_can_be_retried_with_the_same_prompt() {
    let (url, mock) = start_mock().await;
    mock.set_send(
        200,
        json!({
            "promptDispatched": false,
            "promptError": "session is not accepting prompts"
        }),
    );
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let reference = app.blocked_card(&cookie, "ALP", "Which branch?").await;
    let task_id = app.detail(&cookie, &reference).await["id"]
        .as_str()
        .expect("id")
        .to_owned();
    app.state
        .db
        .call(move |connection| {
            connection.execute(
                "UPDATE tasks SET attention = 1, session_status = 'waiting' WHERE id = ?1",
                [&task_id],
            )?;
            Ok(())
        })
        .await
        .expect("seed attention");

    let first = app
        .post(
            &format!("/api/tasks/{reference}/reply"),
            Some(json!({ "body": "main" })),
            &cookie,
        )
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "{:?}", first.body);
    let comment_id = first.body["comment"]["id"]
        .as_str()
        .expect("comment id")
        .to_owned();
    assert_eq!(first.body["delivery"]["status"], "failed");
    assert_eq!(
        first.body["delivery"]["error"],
        "session is not accepting prompts"
    );
    assert_eq!(first.body["comment"]["delivery_status"], "failed");
    assert_eq!(
        first.body["comment"]["delivery_error"],
        "session is not accepting prompts"
    );

    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["status"], "blocker");
    assert_eq!(detail["session_status"], "waiting");
    assert_eq!(detail["attention"], true);
    let kinds: Vec<&str> = detail["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"reply_failed"), "{kinds:?}");
    let failed = detail["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .find(|entry| entry["kind"] == "reply_failed")
        .expect("reply_failed activity");
    assert_eq!(failed["data"]["error"], "session is not accepting prompts");

    let first_prompt = mock.send_requests()[0].body["input"]["prompt"].clone();

    mock.set_send(200, json!({ "promptDispatched": true }));
    let retry = app
        .post(
            &format!("/api/tasks/{reference}/reply/{comment_id}"),
            None,
            &cookie,
        )
        .await;
    assert_eq!(retry.status, StatusCode::OK, "{:?}", retry.body);
    assert_eq!(retry.body["delivery"]["status"], "sent");
    assert_eq!(retry.body["comment"]["delivery_status"], "sent");
    assert!(retry.body["comment"]["delivery_error"].is_null());

    let sends = mock.send_requests();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[1].body["input"]["prompt"], first_prompt);
    assert_eq!(sends[1].body["input"]["sessionId"], "ses_reply");
    assert_eq!(sends[1].body["input"]["directory"], "/tmp/kanso-session");

    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["status"], "blocker");
    assert_eq!(detail["session_status"], "running");
    assert_eq!(detail["attention"], false);
    let kinds: Vec<&str> = detail["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"reply_sent"), "{kinds:?}");
}

#[tokio::test]
async fn retry_works_after_the_card_left_blocker() {
    let (url, mock) = start_mock().await;
    mock.set_send(
        200,
        json!({ "promptDispatched": false, "promptError": "temporarily unavailable" }),
    );
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let reference = app.blocked_card(&cookie, "ALP", "question").await;

    let first = app
        .post(
            &format!("/api/tasks/{reference}/reply"),
            Some(json!({ "body": "answer" })),
            &cookie,
        )
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "{:?}", first.body);
    let comment_id = first.body["comment"]["id"]
        .as_str()
        .expect("comment id")
        .to_owned();
    assert_eq!(first.body["delivery"]["status"], "failed");

    // The agent keeps working elsewhere: the retry only needs an active card.
    let moved = app
        .post(
            &format!("/api/tasks/{reference}/move"),
            Some(json!({ "status": "in_progress" })),
            &cookie,
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);

    mock.set_send(200, json!({ "promptDispatched": true }));
    let retry = app
        .post(
            &format!("/api/tasks/{reference}/reply/{comment_id}"),
            None,
            &cookie,
        )
        .await;
    assert_eq!(retry.status, StatusCode::OK, "{:?}", retry.body);
    assert_eq!(retry.body["delivery"]["status"], "sent");
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["status"], "in_progress");
    assert_eq!(detail["session_status"], "running");
}

#[tokio::test]
async fn a_transport_failure_marks_delivery_unknown_and_leaves_the_card_alone() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind spare port");
    let address = listener.local_addr().expect("spare address");
    drop(listener);

    let app = TestApp::new(Some(&format!("http://{address}"))).await;
    let cookie = app.login().await;
    let package = app.create_project(&cookie, "ALP", "Alpha").await;
    let card = app
        .create_card(&cookie, package["id"].as_str().expect("id"), "Do it")
        .await;
    let reference = card["reference"].as_str().expect("ref");
    app.post(
        &format!("/api/tasks/{reference}/session"),
        Some(json!({
            "session_id": "ses_reply",
            "directory": "/tmp/kanso-session",
            "status": "failed",
        })),
        &cookie,
    )
    .await;
    let moved = app
        .post(
            &format!("/api/tasks/{reference}/move"),
            Some(json!({ "status": "blocker", "reason": "Are you there?" })),
            &cookie,
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);

    let response = app
        .post(
            &format!("/api/tasks/{reference}/reply"),
            Some(json!({ "body": "Yes" })),
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.body);
    assert_eq!(response.body["delivery"]["status"], "unknown");
    assert!(response.body["delivery"]["error"]
        .as_str()
        .expect("error")
        .contains("could not reach OpenChamber"));
    assert_eq!(response.body["comment"]["delivery_status"], "unknown");

    let detail = app.detail(&cookie, reference).await;
    assert_eq!(detail["status"], "blocker");
    assert_eq!(detail["session_status"], "failed");
    let kinds: Vec<&str> = detail["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"reply_unknown"), "{kinds:?}");
    assert!(!kinds.contains(&"reply_sent"), "{kinds:?}");
}

#[tokio::test]
async fn reply_requires_a_blocked_card_and_a_session() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let project_id = project["id"].as_str().expect("id");
    let card = app.create_card(&cookie, project_id, "Do it").await;
    let reference = card["reference"].as_str().expect("ref");
    app.post(
        &format!("/api/tasks/{reference}/session"),
        Some(json!({
            "session_id": "ses_reply",
            "directory": "/tmp/kanso-session",
            "status": "running",
        })),
        &cookie,
    )
    .await;

    let response = app
        .post(
            &format!("/api/tasks/{reference}/reply"),
            Some(json!({ "body": "hello" })),
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(response.code(), "not_blocked");
    assert!(mock.send_requests().is_empty());
    assert!(app.detail(&cookie, reference).await["comments"]
        .as_array()
        .expect("comments")
        .is_empty());

    let moved = app
        .post(
            &format!("/api/tasks/{reference}/move"),
            Some(json!({ "status": "blocker", "reason": "question" })),
            &cookie,
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);
    // Clear only this card's session.
    let task_id = app.detail(&cookie, reference).await["id"]
        .as_str()
        .expect("id")
        .to_owned();
    app.state
        .db
        .call(move |connection| {
            connection.execute(
                "UPDATE tasks SET session_id = NULL, session_directory = NULL WHERE id = ?1",
                [&task_id],
            )?;
            Ok(())
        })
        .await
        .expect("clear session");

    let response = app
        .post(
            &format!("/api/tasks/{reference}/reply"),
            Some(json!({ "body": "hello" })),
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(response.code(), "no_session");
    assert_eq!(mock.send_requests().len(), 0);
}

#[tokio::test]
async fn reply_is_human_only_and_validates_the_body_before_touching_the_card() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let reference = app.blocked_card(&cookie, "ALP", "question").await;

    let token_response = app
        .post(
            "/api/tokens",
            Some(json!({ "name": "build", "scope": "agent" })),
            &cookie,
        )
        .await;
    let token = token_response.body["token"].as_str().expect("token");
    let forbidden = app
        .post_bearer(
            &format!("/api/tasks/{reference}/reply"),
            Some(json!({ "body": "answer" })),
            token,
        )
        .await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
    assert!(mock.send_requests().is_empty());

    for body in [
        json!({ "body": "   " }),
        json!({ "body": "x".repeat(16 * 1024 + 1) }),
    ] {
        let invalid = app
            .post(
                &format!("/api/tasks/{reference}/reply"),
                Some(body),
                &cookie,
            )
            .await;
        assert_eq!(
            invalid.status,
            StatusCode::BAD_REQUEST,
            "{:?}",
            invalid.body
        );
        assert_eq!(invalid.code(), "validation");
    }
    assert!(mock.send_requests().is_empty());
}

#[tokio::test]
async fn reply_is_disabled_with_the_integration_and_rejects_archived_cards() {
    let app = TestApp::new(None).await;
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP", "Alpha").await;
    let card = app
        .create_card(&cookie, project["id"].as_str().expect("id"), "Do it")
        .await;
    let reference = card["reference"].as_str().expect("ref");
    app.post(
        &format!("/api/tasks/{reference}/session"),
        Some(json!({
            "session_id": "ses_reply",
            "directory": "/tmp/kanso-session",
            "status": "running",
        })),
        &cookie,
    )
    .await;
    app.post(
        &format!("/api/tasks/{reference}/move"),
        Some(json!({ "status": "blocker", "reason": "question" })),
        &cookie,
    )
    .await;

    let disabled = app
        .post(
            &format!("/api/tasks/{reference}/reply"),
            Some(json!({ "body": "answer" })),
            &cookie,
        )
        .await;
    assert_eq!(disabled.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(disabled.code(), "integration_disabled");
    assert!(app.detail(&cookie, reference).await["comments"]
        .as_array()
        .expect("comments")
        .is_empty());

    app.post(&format!("/api/tasks/{reference}/archive"), None, &cookie)
        .await;
    let archived = app
        .post(
            &format!("/api/tasks/{reference}/reply"),
            Some(json!({ "body": "answer" })),
            &cookie,
        )
        .await;
    assert_eq!(archived.status, StatusCode::CONFLICT);
    assert_eq!(archived.code(), "not_active");
}

#[tokio::test]
async fn retry_rejects_an_ordinary_comment() {
    let (url, mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let reference = app.blocked_card(&cookie, "ALP", "question").await;
    let comment = app
        .post(
            &format!("/api/tasks/{reference}/comment"),
            Some(json!({ "body": "ordinary" })),
            &cookie,
        )
        .await;
    assert_eq!(comment.status, StatusCode::CREATED);
    let comment_id = comment.body["id"].as_str().expect("comment id");

    let retry = app
        .post(
            &format!("/api/tasks/{reference}/reply/{comment_id}"),
            None,
            &cookie,
        )
        .await;
    assert_eq!(retry.status, StatusCode::CONFLICT);
    assert_eq!(retry.code(), "not_a_reply");
    assert!(mock.send_requests().is_empty());
}

#[tokio::test]
async fn board_cards_and_blocked_activity_carry_the_reason() {
    let (url, _mock) = start_mock().await;
    let app = TestApp::new(Some(&url)).await;
    let cookie = app.login().await;
    let reference = app.blocked_card(&cookie, "ALP", "Which branch?").await;

    let board = app.get("/api/board", &cookie).await;
    let cards = board.body["projects"][0]["columns"]["blocker"]
        .as_array()
        .expect("blocker column");
    let card = cards
        .iter()
        .find(|card| card["reference"] == reference.as_str())
        .expect("blocked card");
    assert_eq!(card["blocked_reason"], "Which branch?");

    let detail = app.detail(&cookie, &reference).await;
    let blocked = detail["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .find(|entry| entry["kind"] == "blocked")
        .expect("blocked activity");
    assert_eq!(blocked["data"]["reason"], "Which branch?");

    let moved = app
        .post(
            &format!("/api/tasks/{reference}/move"),
            Some(json!({ "status": "blocker", "reason": "Use develop only" })),
            &cookie,
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["blocked_reason"], "Use develop only");
    let activity = detail["activity"].as_array().expect("activity");
    let last_moved = activity
        .iter()
        .rev()
        .find(|entry| entry["kind"] == "moved")
        .expect("moved activity");
    assert_eq!(last_moved["data"]["reason"], "Use develop only");
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
async fn poller_keeps_a_busy_then_idle_session_in_progress() {
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
    assert_eq!(detail["status"], "in_progress");
    let kinds: Vec<&str> = detail["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"session_idle"), "{kinds:?}");
    assert!(!kinds.contains(&"auto_moved"), "{kinds:?}");

    // A repeated idle observation does not touch the card again.
    let idle = json!({
        "sessions": [{
            "id": "ses_poll",
            "status": { "type": "idle" },
            "outcome": "succeeded",
            "time": { "created": 1, "busy": 100, "idle": 200 },
        }]
    });
    mock.queue_list(200, idle);
    let activity_count = detail["activity"].as_array().expect("activity").len();
    app.poll().await;
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(
        detail["activity"].as_array().expect("activity").len(),
        activity_count
    );

    // The card is still in_progress, so a resumed session is observed again.
    mock.queue_list(
        200,
        json!({
            "sessions": [{
                "id": "ses_poll",
                "status": { "type": "busy" },
                "time": { "created": 1, "busy": 300 },
            }]
        }),
    );
    app.poll().await;
    let detail = app.detail(&cookie, &reference).await;
    assert_eq!(detail["session_status"], "running");
    assert_eq!(detail["status"], "in_progress");
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
async fn poller_keeps_a_done_card_done_when_the_session_goes_idle() {
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
