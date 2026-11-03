use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use kanso::auth::AuthConfig;
use kanso::db::Database;
use kanso::server::{router, AppState};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse";
const ORIGIN: &str = "https://kanso.example";
static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        for _ in 0..100 {
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("kanso-sse-{}-{sequence}", std::process::id()));
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

struct SseApp {
    _dir: TempDir,
    state: AppState,
    app: Router,
    address: SocketAddr,
    server: JoinHandle<()>,
}

impl SseApp {
    async fn new(heartbeat: Duration) -> Self {
        let dir = TempDir::new();
        let database = Database::open(&dir.path().join("kanso.db")).expect("open database");
        let auth = AuthConfig::load(PASSWORD, Some(ORIGIN), dir.path()).expect("auth config");
        let state = AppState::with_heartbeat(database, dir.path().join("web"), auth, heartbeat);
        let app = router(state.clone());

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("local address");
        let served = app.clone();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, served).await;
        });

        Self {
            _dir: dir,
            state,
            app,
            address,
            server,
        }
    }

    async fn request(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
        cookie: Option<&str>,
        bearer: Option<&str>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mutating = method != Method::GET;
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "kanso.example");
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, format!("kanso_session={cookie}"));
        }
        if let Some(bearer) = bearer {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        if body.is_some() || (cookie.is_some() && mutating) {
            builder = builder.header(header::ORIGIN, ORIGIN);
        }
        let body = body.map_or_else(Body::empty, |value| Body::from(value.to_string()));
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
        (status, headers, body)
    }

    async fn login(&self) -> String {
        let (status, headers, _) = self
            .request(
                Method::POST,
                "/api/auth/login",
                Some(json!({"password": PASSWORD})),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with("kanso_session="))
            .expect("session cookie")
            .split(';')
            .next()
            .and_then(|pair| pair.strip_prefix("kanso_session="))
            .expect("cookie value")
            .to_owned()
    }

    async fn create_token(&self, cookie: &str, name: &str) -> (String, String) {
        let (status, _, body) = self
            .request(
                Method::POST,
                "/api/tokens",
                Some(json!({"name": name, "scope": "agent"})),
                Some(cookie),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body:?}");
        (
            body["id"].as_str().expect("token id").to_owned(),
            body["token"].as_str().expect("token").to_owned(),
        )
    }
}

impl Drop for SseApp {
    fn drop(&mut self) {
        self.server.abort();
    }
}

struct SseClient {
    stream: TcpStream,
    raw: Vec<u8>,
    decoded: Vec<u8>,
    finished: bool,
}

#[derive(Debug)]
struct Frame {
    event: String,
    id: u64,
    data: Value,
}

impl SseClient {
    async fn connect(address: SocketAddr, authorization: &str) -> Self {
        let mut stream = TcpStream::connect(address).await.expect("connect SSE");
        let request = format!(
            "GET /api/events HTTP/1.1\r\nHost: kanso.example\r\n{authorization}Accept: text/event-stream\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write SSE request");
        let mut client = Self {
            stream,
            raw: Vec::new(),
            decoded: Vec::new(),
            finished: false,
        };
        let headers = client
            .read_raw_until(b"\r\n\r\n")
            .await
            .expect("response headers");
        assert!(headers.starts_with("HTTP/1.1 200"), "{headers}");
        assert!(headers.to_ascii_lowercase().contains("text/event-stream"));
        client
    }

    async fn cookie(address: SocketAddr, cookie: &str) -> Self {
        Self::connect(address, &format!("Cookie: kanso_session={cookie}\r\n")).await
    }

    async fn bearer(address: SocketAddr, token: &str) -> Self {
        Self::connect(address, &format!("Authorization: Bearer {token}\r\n")).await
    }

    async fn read_raw_until(&mut self, marker: &[u8]) -> Option<String> {
        loop {
            if let Some(index) = find(&self.raw, marker) {
                let end = index + marker.len();
                let frame = String::from_utf8_lossy(&self.raw[..end]).into_owned();
                self.raw.drain(..end);
                return Some(frame);
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    async fn read_until(&mut self, marker: &[u8]) -> Option<String> {
        loop {
            if dechunk(&mut self.raw, &mut self.decoded) {
                self.finished = true;
            }
            if let Some(index) = find(&self.decoded, marker) {
                let end = index + marker.len();
                let frame = String::from_utf8_lossy(&self.decoded[..end]).into_owned();
                self.decoded.drain(..end);
                return Some(frame);
            }
            if self.finished {
                return None;
            }
            if !self.fill().await {
                self.finished = true;
                if dechunk(&mut self.raw, &mut self.decoded) {
                    self.finished = true;
                }
                if let Some(index) = find(&self.decoded, marker) {
                    let end = index + marker.len();
                    let frame = String::from_utf8_lossy(&self.decoded[..end]).into_owned();
                    self.decoded.drain(..end);
                    return Some(frame);
                }
                return None;
            }
        }
    }

    async fn fill(&mut self) -> bool {
        let mut chunk = [0_u8; 4096];
        let read = tokio::time::timeout(Duration::from_secs(3), self.stream.read(&mut chunk))
            .await
            .expect("SSE read timed out")
            .expect("read SSE");
        if read == 0 {
            return false;
        }
        self.raw.extend_from_slice(&chunk[..read]);
        true
    }

    async fn next_frame(&mut self) -> Option<String> {
        self.read_until(b"\n\n").await
    }

    async fn next_event(&mut self) -> Frame {
        loop {
            let frame = self.next_frame().await.expect("SSE stream ended");
            if let Some(event) = parse_frame(&frame) {
                return event;
            }
        }
    }
}

/// Decode every complete HTTP chunk in `raw` into `decoded`. Returns true once
/// the terminating zero-length chunk is seen.
fn dechunk(raw: &mut Vec<u8>, decoded: &mut Vec<u8>) -> bool {
    loop {
        let Some(line_end) = find(raw, b"\r\n") else {
            return false;
        };
        let Ok(size_text) = std::str::from_utf8(&raw[..line_end]) else {
            return false;
        };
        let Ok(size) = usize::from_str_radix(size_text.trim(), 16) else {
            return false;
        };
        if size == 0 {
            let end = (line_end + 4).min(raw.len());
            raw.drain(..end);
            return true;
        }
        let end = line_end + 2 + size + 2;
        if raw.len() < end {
            return false;
        }
        decoded.extend_from_slice(&raw[line_end + 2..line_end + 2 + size]);
        raw.drain(..end);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_frame(frame: &str) -> Option<Frame> {
    let mut event = None;
    let mut id = None;
    let mut data = String::new();
    for line in frame.lines() {
        if let Some(value) = line.strip_prefix("event: ") {
            event = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("id: ") {
            id = Some(value.parse().expect("numeric event id"));
        } else if let Some(value) = line.strip_prefix("data: ") {
            data.push_str(value);
        }
    }
    Some(Frame {
        event: event?,
        id: id?,
        data: serde_json::from_str(&data).expect("event data JSON"),
    })
}

#[tokio::test]
async fn sse_requires_auth_and_accepts_cookie_and_token() {
    let app = SseApp::new(Duration::from_millis(100)).await;

    let headers = raw_headers(
        app.address,
        "GET /api/events HTTP/1.1\r\nHost: kanso.example\r\n\r\n",
    )
    .await;
    assert!(headers.starts_with("HTTP/1.1 401"), "{headers}");

    let cookie = app.login().await;
    let (_token_id, token) = app.create_token(&cookie, "build").await;
    let mut cookie_client = SseClient::cookie(app.address, &cookie).await;
    let mut token_client = SseClient::bearer(app.address, &token).await;

    let published = app
        .state
        .events
        .publish("task.created", json!({"task": {"reference": "ALP-1"}}));
    let first = cookie_client.next_event().await;
    let second = token_client.next_event().await;
    for frame in [&first, &second] {
        assert_eq!(frame.event, "task.created");
        assert_eq!(frame.id, published.id);
        assert_eq!(frame.data["id"], published.id);
        assert_eq!(frame.data["type"], "task.created");
        assert_eq!(frame.data["data"]["task"]["reference"], "ALP-1");
    }

    let published = app
        .state
        .events
        .publish("task.moved", json!({"task": {"reference": "ALP-1"}}));
    assert!(published.id > first.id);
    assert_eq!(cookie_client.next_event().await.id, published.id);
}

#[tokio::test]
async fn sse_heartbeat_survives_an_idle_read_and_disconnects_cleanly() {
    let app = SseApp::new(Duration::from_millis(50)).await;
    let cookie = app.login().await;

    let mut client = SseClient::cookie(app.address, &cookie).await;
    let frame = client.next_frame().await.expect("heartbeat");
    assert!(
        frame.starts_with(": heartbeat") || frame.starts_with(":heartbeat"),
        "{frame:?}"
    );
    drop(client);

    // The server must still accept new streams and publish after a disconnect.
    let mut second = SseClient::cookie(app.address, &cookie).await;
    let published = app
        .state
        .events
        .publish("task.updated", json!({"task": {"reference": "ALP-1"}}));
    assert_eq!(second.next_event().await.id, published.id);
}

#[tokio::test]
async fn revoked_tokens_stop_existing_streams() {
    let app = SseApp::new(Duration::from_millis(50)).await;
    let cookie = app.login().await;
    let (token_id, token) = app.create_token(&cookie, "build").await;
    let mut client = SseClient::bearer(app.address, &token).await;

    let (status, _, body) = app
        .request(
            Method::DELETE,
            &format!("/api/tokens/{token_id}"),
            None,
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let mut closed = false;
    for _ in 0..20 {
        if client.next_frame().await.is_none() {
            closed = true;
            break;
        }
    }
    assert!(closed, "revoked token stream should close");
}

#[tokio::test]
async fn logout_stops_existing_streams() {
    let app = SseApp::new(Duration::from_millis(50)).await;
    let cookie = app.login().await;
    let mut client = SseClient::cookie(app.address, &cookie).await;

    let (status, _, body) = app
        .request(Method::POST, "/api/auth/logout", None, Some(&cookie), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let mut closed = false;
    for _ in 0..20 {
        if client.next_frame().await.is_none() {
            closed = true;
            break;
        }
    }
    assert!(closed, "logged-out session stream should close");
}

#[tokio::test]
async fn expired_sessions_stop_existing_streams() {
    let app = SseApp::new(Duration::from_millis(50)).await;
    let cookie = app.login().await;
    let mut client = SseClient::cookie(app.address, &cookie).await;

    let session_id = cookie.split('.').next().expect("session id").to_owned();
    app.state
        .db
        .call(move |connection| {
            connection.execute(
                "UPDATE ui_sessions SET expires_at = 1 WHERE id = ?1",
                [session_id],
            )?;
            Ok(())
        })
        .await
        .expect("expire session");

    let mut closed = false;
    for _ in 0..20 {
        if client.next_frame().await.is_none() {
            closed = true;
            break;
        }
    }
    assert!(closed, "expired session stream should close");
}

#[tokio::test]
async fn mutations_publish_events_after_commit() {
    let app = SseApp::new(Duration::from_millis(100)).await;
    let cookie = app.login().await;

    let (status, _, project) = app
        .request(
            Method::POST,
            "/api/projects",
            Some(json!({"key": "ALP", "name": "Alpha"})),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{project:?}");

    let mut client = SseClient::cookie(app.address, &cookie).await;

    let (status, _, task) = app
        .request(
            Method::POST,
            "/api/tasks",
            Some(json!({"project_key": "ALP", "title": "Live"})),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{task:?}");
    let created = client.next_event().await;
    assert_eq!(created.event, "task.created");
    assert_eq!(created.data["data"]["task"]["reference"], "ALP-1");

    let reference = task["reference"].as_str().expect("reference");
    let (status, _, _) = app
        .request(
            Method::PATCH,
            &format!("/api/tasks/{reference}"),
            Some(json!({"title": "Renamed"})),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let updated = client.next_event().await;
    assert_eq!(updated.event, "task.updated");
    assert_eq!(updated.data["data"]["task"]["title"], "Renamed");

    let (status, _, _) = app
        .request(
            Method::POST,
            &format!("/api/tasks/{reference}/archive"),
            None,
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let archived = client.next_event().await;
    assert_eq!(archived.event, "task.archived");
    assert_eq!(archived.data["data"]["task"]["state"], "archived");
}

async fn raw_headers(address: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(address).await.expect("connect raw");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write raw request");
    let mut buffer = Vec::new();
    loop {
        if let Some(index) = find(&buffer, b"\r\n\r\n") {
            let end = index + 4;
            return String::from_utf8_lossy(&buffer[..end]).into_owned();
        }
        let mut chunk = [0_u8; 4096];
        let read = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut chunk))
            .await
            .expect("raw read timed out")
            .expect("read raw");
        if read == 0 {
            return String::from_utf8_lossy(&buffer).into_owned();
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}
