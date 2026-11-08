use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::{to_bytes, Body};
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use kanso::auth::AuthConfig;
use kanso::db::Database;
use kanso::server::{router, AppState};
use serde_json::{json, Value};
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
                std::env::temp_dir().join(format!("kanso-api-{}-{sequence}", std::process::id()));
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

    fn set_cookie(&self) -> Option<&str> {
        self.headers
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
    }
}

impl TestApp {
    fn new() -> Self {
        let dir = TempDir::new();
        let database = Database::open(&dir.path().join("kanso.db")).expect("open database");
        let auth = AuthConfig::load(PASSWORD, Some(ORIGIN), dir.path()).expect("load auth config");
        let state = AppState::new(database, dir.path().join("web"), auth);
        let app = router(state.clone())
            .layer(MockConnectInfo(SocketAddr::from(([203, 0, 113, 7], 4444))));
        Self {
            _dir: dir,
            state,
            app,
        }
    }

    async fn request(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
        cookie: Option<&str>,
        bearer: Option<&str>,
        origin: Option<&str>,
    ) -> ApiResponse {
        self.request_with_headers(method, uri, body, cookie, bearer, origin, &[])
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn request_with_headers(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
        cookie: Option<&str>,
        bearer: Option<&str>,
        origin: Option<&str>,
        extra: &[(&str, &str)],
    ) -> ApiResponse {
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
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        for (name, value) in extra {
            builder = builder.header(*name, *value);
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

    async fn get(&self, uri: &str, cookie: Option<&str>, bearer: Option<&str>) -> ApiResponse {
        self.request(Method::GET, uri, None, cookie, bearer, None)
            .await
    }

    async fn cookie_post(&self, uri: &str, body: Option<Value>, cookie: &str) -> ApiResponse {
        self.request(Method::POST, uri, body, Some(cookie), None, Some(ORIGIN))
            .await
    }

    async fn cookie_patch(&self, uri: &str, body: Value, cookie: &str) -> ApiResponse {
        self.request(
            Method::PATCH,
            uri,
            Some(body),
            Some(cookie),
            None,
            Some(ORIGIN),
        )
        .await
    }

    async fn cookie_delete(&self, uri: &str, cookie: &str) -> ApiResponse {
        self.request(Method::DELETE, uri, None, Some(cookie), None, Some(ORIGIN))
            .await
    }

    async fn login(&self) -> String {
        let response = self
            .request(
                Method::POST,
                "/api/auth/login",
                Some(json!({"password": PASSWORD})),
                None,
                None,
                None,
            )
            .await;
        assert_eq!(
            response.status,
            StatusCode::NO_CONTENT,
            "{:?}",
            response.body
        );
        response.session_cookie()
    }

    async fn create_project(&self, cookie: &str, key: &str) -> Value {
        let response = self
            .cookie_post(
                "/api/projects",
                Some(json!({"key": key, "name": key})),
                cookie,
            )
            .await;
        assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.body);
        response.body
    }

    async fn create_task(&self, cookie: &str, body: Value) -> Value {
        let response = self.cookie_post("/api/tasks", Some(body), cookie).await;
        assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.body);
        response.body
    }

    async fn create_label(&self, cookie: &str, name: &str, color: &str) -> Value {
        let response = self
            .cookie_post(
                "/api/labels",
                Some(json!({"name": name, "color": color})),
                cookie,
            )
            .await;
        assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.body);
        response.body
    }

    async fn create_token(&self, cookie: &str, name: &str, scope: &str) -> Value {
        let response = self
            .cookie_post(
                "/api/tokens",
                Some(json!({"name": name, "scope": scope})),
                cookie,
            )
            .await;
        assert_eq!(response.status, StatusCode::CREATED, "{:?}", response.body);
        response.body
    }

    async fn db_i64(&self, sql: &str, parameters: Vec<Value>) -> i64 {
        let sql = sql.to_owned();
        self.state
            .db
            .call(move |connection| {
                let values: Vec<rusqlite::types::Value> = parameters
                    .into_iter()
                    .map(|value| match value {
                        Value::String(text) => rusqlite::types::Value::Text(text),
                        Value::Number(number) => {
                            rusqlite::types::Value::Integer(number.as_i64().unwrap_or(0))
                        }
                        _ => rusqlite::types::Value::Null,
                    })
                    .collect();
                connection
                    .query_row(&sql, rusqlite::params_from_iter(values), |row| {
                        row.get::<_, i64>(0)
                    })
                    .map_err(Into::into)
            })
            .await
            .expect("query scalar")
    }

    async fn read_last_seen(&self, session_id: &str) -> i64 {
        let session_id = session_id.to_owned();
        self.state
            .db
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT last_seen FROM ui_sessions WHERE id = ?1",
                        [session_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(Into::into)
            })
            .await
            .expect("last_seen")
    }

    async fn set_expired(&self, session_id: &str) {
        let session_id = session_id.to_owned();
        self.state
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
    }
}

fn session_id(cookie: &str) -> &str {
    cookie.split('.').next().unwrap_or(cookie)
}

fn activity_kinds(task: &Value) -> Vec<String> {
    task["activity"]
        .as_array()
        .expect("activity array")
        .iter()
        .filter_map(|entry| entry["kind"].as_str().map(str::to_owned))
        .collect()
}

struct TestImportProvider;

impl kanso::importer::Provider for TestImportProvider {
    fn info(&self) -> kanso::importer::ProviderInfo {
        use kanso::importer::{Filter, ProviderInfo, Source};
        ProviderInfo {
            id: "fixture".into(),
            name: "Fixture".into(),
            sources: vec![
                Source {
                    id: "one".into(),
                    name: "One".into(),
                },
                Source {
                    id: "two".into(),
                    name: "Two".into(),
                },
            ],
            filters: vec![Filter {
                id: "query".into(),
                label: "Query".into(),
            }],
        }
    }

    fn preview(
        &self,
        _: &str,
        filters: &std::collections::HashMap<String, String>,
    ) -> Result<Vec<kanso::importer::Candidate>, kanso::importer::ProviderError> {
        let query = filters.get("query").map(String::as_str).unwrap_or("");
        if query == "fail" {
            return Err(kanso::importer::ProviderError);
        }
        let candidate = |identity: &str, title: &str, url: &str| kanso::importer::Candidate {
            identity: identity.into(),
            kind: "issue".into(),
            title: title.into(),
            description: "<script>alert(1)</script> **hello**".into(),
            source_url: url.into(),
        };
        let candidates = vec![
            candidate("stable", "Original", "https://example.com/item"),
            candidate("bad", "Unsafe", "javascript:alert(1)"),
            candidate("long", &"x".repeat(201), "https://example.com/item2"),
            candidate("other", "Other", "https://example.com/other"),
        ];
        match query {
            "count-limit" => Ok(vec![candidates[0].clone(); 200]),
            "too-many" => Ok(vec![candidates[0].clone(); 201]),
            "too-large" => {
                let mut item = candidates[0].clone();
                item.description = "x".repeat(2 * 1024 * 1024);
                Ok(vec![item])
            }
            "size-limit" => {
                let mut item = candidates[0].clone();
                let other_bytes = item.identity.len()
                    + item.kind.len()
                    + item.title.len()
                    + item.source_url.len();
                item.description = "x".repeat(2 * 1024 * 1024 - other_bytes);
                Ok(vec![item])
            }
            "field-limits" => {
                let mut items = Vec::new();
                let mut item = candidates[0].clone();
                item.description = "x".repeat(64 * 1024);
                items.push(item.clone());
                item.description.push('x');
                items.push(item.clone());
                item.description.clear();
                item.identity = "x".repeat(513);
                items.push(item.clone());
                item.identity = "ok".into();
                item.kind = "x".repeat(129);
                items.push(item);
                Ok(items)
            }
            _ => Ok(candidates),
        }
    }
}

fn import_app(provider: std::sync::Arc<dyn kanso::importer::Provider>) -> TestApp {
    let mut app = TestApp::new();
    app.state = app.state.clone().with_import_providers(vec![provider]);
    app.app = router(app.state.clone())
        .layer(MockConnectInfo(SocketAddr::from(([203, 0, 113, 7], 4444))));
    app
}

fn import_preview_request(project: &str, query: &str) -> Value {
    json!({
        "project_id": project,
        "provider": "fixture",
        "source": "one",
        "filters": { "query": query }
    })
}

fn import_confirm_request(project: &str, preview_id: &Value) -> Value {
    json!({
        "preview_id": preview_id,
        "project_id": project,
        "provider": "fixture",
        "source": "one",
        "selected": [0]
    })
}

#[tokio::test]
async fn import_mutations_enforce_auth_csrf_and_snapshot_ownership() {
    let app = import_app(std::sync::Arc::new(TestImportProvider));
    let cookie = app.login().await;
    let other_cookie = app.login().await;
    let project = app.create_project(&cookie, "OWN").await;
    let id = project["id"].as_str().unwrap();
    let agent = app.create_token(&cookie, "import-agent", "agent").await;
    let agent = agent["token"].as_str().unwrap();
    let preview_request = import_preview_request(id, "");

    let preview = app
        .cookie_post(
            "/api/imports/preview",
            Some(preview_request.clone()),
            &cookie,
        )
        .await;
    assert_eq!(preview.status, StatusCode::OK);
    let confirm_request = import_confirm_request(id, &preview.body["preview_id"]);

    for (path, body) in [
        ("/api/imports/preview", preview_request),
        ("/api/imports/confirm", confirm_request.clone()),
    ] {
        let anonymous = app
            .request(Method::POST, path, Some(body.clone()), None, None, None)
            .await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{path}");
        let agent_response = app
            .request(
                Method::POST,
                path,
                Some(body.clone()),
                None,
                Some(agent),
                None,
            )
            .await;
        assert_eq!(agent_response.status, StatusCode::FORBIDDEN, "{path}");
        let csrf = app
            .request(
                Method::POST,
                path,
                Some(body),
                Some(&cookie),
                None,
                Some("https://evil.example"),
            )
            .await;
        assert_eq!(csrf.code(), "csrf", "{path}");
        assert_eq!(csrf.status, StatusCode::FORBIDDEN, "{path}");
    }

    let cross_owner = app
        .cookie_post(
            "/api/imports/confirm",
            Some(confirm_request.clone()),
            &other_cookie,
        )
        .await;
    assert_eq!(cross_owner.status, StatusCode::CONFLICT);
    for field in ["project_id", "provider", "source"] {
        let mut mismatch = confirm_request.clone();
        mismatch[field] = json!("different");
        let response = app
            .cookie_post("/api/imports/confirm", Some(mismatch), &cookie)
            .await;
        assert_eq!(response.status, StatusCode::CONFLICT, "{field}");
    }
    for field in [
        "candidate",
        "candidates",
        "title",
        "identity",
        "description",
        "source_url",
    ] {
        let mut forged = confirm_request.clone();
        forged[field] = json!("attacker-controlled");
        let response = app
            .cookie_post("/api/imports/confirm", Some(forged), &cookie)
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{field}");
    }
    assert_eq!(app.db_i64("SELECT count(*) FROM tasks", vec![]).await, 0);
    let accepted = app
        .cookie_post("/api/imports/confirm", Some(confirm_request), &cookie)
        .await;
    assert_eq!(accepted.body[0]["status"], "created");
}

#[tokio::test]
async fn import_preview_rejects_invalid_controls_and_invalidates_old_snapshots() {
    let app = import_app(std::sync::Arc::new(TestImportProvider));
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "CTL").await;
    let id = project["id"].as_str().unwrap();

    let invalid = [
        ("provider", json!("unknown")),
        ("source", json!("unknown")),
        ("project_id", json!("unknown")),
        ("filters", json!({"unknown": "value"})),
        ("filters", json!({"query": "x".repeat(1025)})),
        ("filters", json!({"query": "bad\nvalue"})),
    ];
    for (field, value) in invalid {
        let preview = app
            .cookie_post(
                "/api/imports/preview",
                Some(import_preview_request(id, "")),
                &cookie,
            )
            .await;
        assert_eq!(preview.status, StatusCode::OK);
        let confirm = import_confirm_request(id, &preview.body["preview_id"]);
        let mut bad = import_preview_request(id, "");
        bad[field] = value;
        assert_eq!(
            app.cookie_post("/api/imports/preview", Some(bad), &cookie)
                .await
                .status,
            StatusCode::BAD_REQUEST,
            "{field}"
        );
        assert_eq!(
            app.cookie_post("/api/imports/confirm", Some(confirm), &cookie)
                .await
                .status,
            StatusCode::CONFLICT,
            "old preview after invalid {field}"
        );
    }
    let old = app
        .cookie_post(
            "/api/imports/preview",
            Some(import_preview_request(id, "")),
            &cookie,
        )
        .await;
    let old_confirm = import_confirm_request(id, &old.body["preview_id"]);
    let failed = app
        .cookie_post(
            "/api/imports/preview",
            Some(import_preview_request(id, "fail")),
            &cookie,
        )
        .await;
    assert_eq!(failed.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        app.cookie_post("/api/imports/confirm", Some(old_confirm), &cookie)
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(app.db_i64("SELECT count(*) FROM tasks", vec![]).await, 0);
}

#[tokio::test]
async fn import_preview_and_selection_limits_are_enforced() {
    let app = import_app(std::sync::Arc::new(TestImportProvider));
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "LIM").await;
    let id = project["id"].as_str().unwrap();

    let limit = app
        .cookie_post(
            "/api/imports/preview",
            Some(import_preview_request(id, "count-limit")),
            &cookie,
        )
        .await;
    assert_eq!(limit.status, StatusCode::OK);
    assert_eq!(limit.body["candidates"].as_array().unwrap().len(), 200);
    let mut too_many_selected = import_confirm_request(id, &limit.body["preview_id"]);
    too_many_selected["selected"] = json!((0..201).collect::<Vec<_>>());
    assert_eq!(
        app.cookie_post("/api/imports/confirm", Some(too_many_selected), &cookie)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );

    for query in ["too-many", "too-large"] {
        let response = app
            .cookie_post(
                "/api/imports/preview",
                Some(import_preview_request(id, query)),
                &cookie,
            )
            .await;
        assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE, "{query}");
        assert_eq!(response.code(), "provider_error");
    }
    let size_limit = app
        .cookie_post(
            "/api/imports/preview",
            Some(import_preview_request(id, "size-limit")),
            &cookie,
        )
        .await;
    assert_eq!(size_limit.status, StatusCode::OK);
    // The aggregate preview fits, but the individual description is invalid.
    assert_eq!(size_limit.body["candidates"][0]["valid"], false);
    let fields = app
        .cookie_post(
            "/api/imports/preview",
            Some(import_preview_request(id, "field-limits")),
            &cookie,
        )
        .await;
    assert_eq!(fields.status, StatusCode::OK);
    let candidates = fields.body["candidates"].as_array().unwrap();
    assert_eq!(
        candidates
            .iter()
            .map(|c| c["valid"].as_bool().unwrap())
            .collect::<Vec<_>>(),
        [true, false, false, false]
    );
    assert_eq!(app.db_i64("SELECT count(*) FROM tasks", vec![]).await, 0);
}

struct SlowImportProvider {
    started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl kanso::importer::Provider for SlowImportProvider {
    fn info(&self) -> kanso::importer::ProviderInfo {
        kanso::importer::Provider::info(&TestImportProvider)
    }

    fn preview(
        &self,
        source: &str,
        filters: &std::collections::HashMap<String, String>,
    ) -> Result<Vec<kanso::importer::Candidate>, kanso::importer::ProviderError> {
        if filters.get("query").is_some_and(|value| value == "slow") {
            if let Some(started) = self.started.lock().unwrap().take() {
                let _ = started.send(());
            }
            self.release
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(5))
                .map_err(|_| kanso::importer::ProviderError)?;
        }
        kanso::importer::Provider::preview(&TestImportProvider, source, filters)
    }
}

#[tokio::test]
async fn import_older_preview_cannot_replace_newer_out_of_order_result() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let provider = SlowImportProvider {
        started: std::sync::Mutex::new(Some(started_tx)),
        release: std::sync::Mutex::new(release_rx),
    };
    let app = import_app(std::sync::Arc::new(provider));
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "RAC").await;
    let id = project["id"].as_str().unwrap();

    let slow = app.cookie_post(
        "/api/imports/preview",
        Some(import_preview_request(id, "slow")),
        &cookie,
    );
    let newer = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), started_rx)
            .await
            .expect("slow provider did not start")
            .expect("slow provider dropped signal");
        let response = app
            .cookie_post(
                "/api/imports/preview",
                Some(import_preview_request(id, "")),
                &cookie,
            )
            .await;
        release_tx.send(()).unwrap();
        response
    };
    let (slow, newer) = tokio::join!(slow, newer);
    assert_eq!(slow.status, StatusCode::CONFLICT);
    assert_eq!(newer.status, StatusCode::OK);
    let confirm = import_confirm_request(id, &newer.body["preview_id"]);
    let result = app
        .cookie_post("/api/imports/confirm", Some(confirm), &cookie)
        .await;
    assert_eq!(result.body[0]["status"], "created");
}

#[tokio::test]
async fn imports_auth_preview_confirm_and_provenance() {
    let mut app = TestApp::new();
    app.state = app
        .state
        .clone()
        .with_import_providers(vec![std::sync::Arc::new(TestImportProvider)]);
    app.app = router(app.state.clone())
        .layer(MockConnectInfo(SocketAddr::from(([203, 0, 113, 7], 4444))));
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "IMP").await;
    let id = project["id"].as_str().unwrap();
    let agent = app.create_token(&cookie, "import-agent", "agent").await;
    let agent = agent["token"].as_str().unwrap();
    let human = app.create_token(&cookie, "import-human", "human").await;
    assert_eq!(
        app.get("/api/imports/providers", None, human["token"].as_str())
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.get("/api/imports/providers", None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.get("/api/imports/providers", None, Some(agent))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.get("/api/imports/providers", Some(&cookie), None)
            .await
            .body[0]["id"],
        "fixture"
    );
    let input = import_preview_request(id, "");
    assert_eq!(
        app.request(
            Method::POST,
            "/api/imports/preview",
            Some(input.clone()),
            Some(&cookie),
            None,
            Some("https://evil.example")
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    let failed = app
        .cookie_post(
            "/api/imports/preview",
            Some(import_preview_request(id, "fail")),
            &cookie,
        )
        .await;
    assert_eq!(failed.status, StatusCode::SERVICE_UNAVAILABLE);
    let preview = app
        .cookie_post("/api/imports/preview", Some(input.clone()), &cookie)
        .await;
    assert_eq!(preview.status, StatusCode::OK, "{:?}", preview.body);
    assert_eq!(preview.body["candidates"][1]["valid"], false);
    assert_eq!(preview.body["candidates"][2]["valid"], false);
    assert_eq!(app.db_i64("SELECT count(*) FROM tasks", vec![]).await, 0);
    let preview_id = preview.body["preview_id"].as_str().unwrap();
    let mut confirm = import_confirm_request(id, &json!(preview_id));
    confirm["selected"] = json!([0, 1, 2]);
    let mut forged = import_confirm_request(id, &json!(preview_id));
    forged["source"] = json!("two");
    assert_eq!(
        app.cookie_post("/api/imports/confirm", Some(forged), &cookie)
            .await
            .status,
        StatusCode::CONFLICT
    );
    let mut duplicate = import_confirm_request(id, &json!(preview_id));
    duplicate["selected"] = json!([0, 0]);
    assert_eq!(
        app.cookie_post("/api/imports/confirm", Some(duplicate), &cookie)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let mut empty = import_confirm_request(id, &json!(preview_id));
    empty["selected"] = json!([]);
    assert_eq!(
        app.cookie_post("/api/imports/confirm", Some(empty), &cookie)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let mut invented = import_confirm_request(id, &json!(preview_id));
    invented["selected"] = json!([99]);
    assert_eq!(
        app.cookie_post("/api/imports/confirm", Some(invented), &cookie)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let mut events = app.state.events.subscribe();
    let (first, second) = tokio::join!(
        app.cookie_post("/api/imports/confirm", Some(confirm.clone()), &cookie),
        app.cookie_post("/api/imports/confirm", Some(confirm.clone()), &cookie)
    );
    let statuses = [
        first.body[0]["status"].as_str().unwrap(),
        second.body[0]["status"].as_str().unwrap(),
    ];
    assert!(statuses.contains(&"created") && statuses.contains(&"skipped"));
    assert_eq!(first.body[1]["status"], "failed");
    assert_eq!(first.body[2]["status"], "failed");
    let event = events.try_recv().expect("created event");
    assert_eq!(event.kind, "task.created");
    assert!(events.try_recv().is_err());
    assert_eq!(app.db_i64("SELECT count(*) FROM tasks", vec![]).await, 1);
    let reference = first.body[0]["reference"].as_str().unwrap();
    let detail = app
        .get(&format!("/api/tasks/{reference}"), Some(&cookie), None)
        .await
        .body;
    assert_eq!(detail["provenance"]["identity"], "stable");
    assert_eq!(detail["source"], "human");
    assert_eq!(detail["status"], "backlog");
    assert_eq!(detail["description"], "<script>alert(1)</script> **hello**");
    assert!(activity_kinds(&detail).contains(&"created".to_string()));
    assert_eq!(
        app.get("/api/export.json", Some(&cookie), None).await.body["tasks"][0]["provenance"]
            ["source_url"],
        "https://example.com/item"
    );
    app.state
        .db
        .call(|db| {
            db.execute_batch(
                "CREATE TRIGGER import_test_failure BEFORE INSERT ON task_provenance
                 WHEN NEW.identity = 'other'
                 BEGIN SELECT RAISE(ABORT, 'fixture'); END;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut other_item = import_confirm_request(id, &json!(preview_id));
    other_item["selected"] = json!([3]);
    assert_eq!(
        app.cookie_post("/api/imports/confirm", Some(other_item.clone()), &cookie)
            .await
            .body[0]["status"],
        "failed"
    );
    assert_eq!(app.db_i64("SELECT count(*) FROM tasks", vec![]).await, 1);
    app.state
        .db
        .call(|db| {
            db.execute_batch("DROP TRIGGER import_test_failure")?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        app.cookie_post("/api/imports/confirm", Some(other_item), &cookie)
            .await
            .body[0]["status"],
        "created"
    );
    app.cookie_patch(
        &format!("/api/tasks/{reference}"),
        json!({"title":"Local edit"}),
        &cookie,
    )
    .await;
    app.cookie_post(&format!("/api/tasks/{reference}/archive"), None, &cookie)
        .await;
    let again = app
        .cookie_post("/api/imports/confirm", Some(confirm), &cookie)
        .await;
    assert_eq!(again.body[0]["status"], "skipped");
    assert_eq!(
        app.get(&format!("/api/tasks/{reference}"), Some(&cookie), None)
            .await
            .body["title"],
        "Local edit"
    );
    let newer = app
        .cookie_post("/api/imports/preview", Some(input), &cookie)
        .await;
    assert_eq!(newer.body["candidates"][0]["existing"], reference);
    let mut stale = import_confirm_request(id, &json!(preview_id));
    stale["selected"] = json!([3]);
    assert_eq!(
        app.cookie_post("/api/imports/confirm", Some(stale), &cookie)
            .await
            .status,
        StatusCode::CONFLICT
    );
    let project2 = app.create_project(&cookie, "TWO").await;
    let id2 = project2["id"].as_str().unwrap();
    let mut other_request = import_preview_request(id2, "");
    other_request["source"] = json!("two");
    let other = app
        .cookie_post("/api/imports/preview", Some(other_request), &cookie)
        .await;
    let mut different = import_confirm_request(id2, &other.body["preview_id"]);
    different["source"] = json!("two");
    assert_eq!(
        app.cookie_post("/api/imports/confirm", Some(different), &cookie)
            .await
            .body[0]["status"],
        "created"
    );
    app.cookie_post(&format!("/api/projects/{id2}/archive"), None, &cookie)
        .await;
    assert_eq!(
        app.cookie_post(
            "/api/imports/preview",
            Some(import_preview_request(id2, "")),
            &cookie
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.cookie_post(
            "/api/imports/preview",
            Some(import_preview_request("missing", "")),
            &cookie
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn login_success_failure_and_rate_limit() {
    let app = TestApp::new();

    let response = app
        .request(
            Method::POST,
            "/api/auth/login",
            Some(json!({"password": "wrong"})),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(response.code(), "unauthorized");

    let response = app
        .request(
            Method::POST,
            "/api/auth/login",
            Some(json!({"password": PASSWORD})),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);
    let set_cookie = response.set_cookie().expect("Set-Cookie");
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Strict"));
    assert!(set_cookie.contains("Path=/"));
    assert!(set_cookie.contains("Max-Age=2592000"));
    assert!(set_cookie.contains("Secure"));
    let cookie = response.session_cookie();

    let me = app.get("/api/me", Some(&cookie), None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["kind"], "ui");
    assert_eq!(me.body["name"], "ui");
    assert_eq!(me.body["scope"], "human");

    for _ in 0..5 {
        let response = app
            .request(
                Method::POST,
                "/api/auth/login",
                Some(json!({"password": "wrong"})),
                None,
                None,
                None,
            )
            .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    }
    let limited = app
        .request(
            Method::POST,
            "/api/auth/login",
            Some(json!({"password": PASSWORD})),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.code(), "rate_limited");
}

#[tokio::test]
async fn cookies_tampering_logout_and_expiry() {
    let app = TestApp::new();
    let cookie = app.login().await;

    let mut tampered = cookie.clone();
    tampered.push('x');
    let response = app.get("/api/me", Some(&tampered), None).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert!(response
        .set_cookie()
        .unwrap_or_default()
        .contains("Max-Age=0"));

    let cookie = app.login().await;
    let response = app
        .request(
            Method::POST,
            "/api/auth/logout",
            None,
            Some(&cookie),
            None,
            Some(ORIGIN),
        )
        .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);
    assert!(response
        .set_cookie()
        .unwrap_or_default()
        .contains("Max-Age=0"));
    let response = app.get("/api/me", Some(&cookie), None).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);

    let cookie = app.login().await;
    let id = session_id(&cookie).to_owned();
    let last_seen = app.read_last_seen(&id).await;
    let _ = app.get("/api/me", Some(&cookie), None).await;
    assert_eq!(last_seen, app.read_last_seen(&id).await);

    let session = id.clone();
    app.state
        .db
        .call(move |connection| {
            connection.execute(
                "UPDATE ui_sessions SET last_seen = 1 WHERE id = ?1",
                [session],
            )?;
            Ok(())
        })
        .await
        .expect("age session");
    let _ = app.get("/api/me", Some(&cookie), None).await;
    assert!(app.read_last_seen(&id).await > 1);

    app.set_expired(session_id(&cookie)).await;
    let response = app.get("/api/me", Some(&cookie), None).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert!(response
        .set_cookie()
        .unwrap_or_default()
        .contains("Max-Age=0"));
}

#[tokio::test]
async fn csrf_rules_for_cookie_and_token_requests() {
    let app = TestApp::new();
    let cookie = app.login().await;

    let response = app
        .request(
            Method::POST,
            "/api/projects",
            Some(json!({"key": "ALP", "name": "Alpha"})),
            Some(&cookie),
            None,
            Some("https://evil.example"),
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.code(), "csrf");

    let response = app
        .request(
            Method::POST,
            "/api/projects",
            Some(json!({"key": "ALP", "name": "Alpha"})),
            Some(&cookie),
            None,
            Some(ORIGIN),
        )
        .await;
    assert_eq!(response.status, StatusCode::CREATED);

    let response = app
        .request(
            Method::POST,
            "/api/auth/logout",
            None,
            Some(&cookie),
            None,
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.code(), "csrf");

    let token = app.create_token(&cookie, "human-cli", "human").await;
    let token = token["token"].as_str().expect("token");
    let response = app
        .request(
            Method::POST,
            "/api/projects",
            Some(json!({"key": "BET", "name": "Beta"})),
            None,
            Some(token),
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::CREATED);
}

#[tokio::test]
async fn token_lifecycle_auth_and_last_used() {
    let app = TestApp::new();
    let cookie = app.login().await;

    let created = app.create_token(&cookie, "build", "agent").await;
    let token = created["token"].as_str().expect("token").to_owned();
    assert!(token.starts_with("kanso_"));
    assert_eq!(token.len(), 49);
    assert!(created.get("hash").is_none());

    let list = app.get("/api/tokens", Some(&cookie), None).await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(list.body[0]["name"], "build");
    assert_eq!(list.body[0]["scope"], "agent");
    assert!(list.body[0].get("hash").is_none());
    assert!(list.body[0].get("token").is_none());
    assert!(list.body[0]["last_used_at"].is_null());
    let stored = {
        let id = created["id"].as_str().expect("id").to_owned();
        app.state
            .db
            .call(move |connection| {
                connection
                    .query_row("SELECT hash FROM tokens WHERE id = ?1", [id], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(Into::into)
            })
            .await
            .expect("stored hash")
    };
    assert_eq!(stored.len(), 64);
    assert_ne!(stored, token);

    let me = app.get("/api/me", None, Some(&token)).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["kind"], "token");
    assert_eq!(me.body["name"], "build");
    assert_eq!(me.body["scope"], "agent");

    let list = app.get("/api/tokens", Some(&cookie), None).await;
    let first_used = list.body[0]["last_used_at"].as_i64().expect("last_used_at");
    let _ = app.get("/api/me", None, Some(&token)).await;
    let list = app.get("/api/tokens", Some(&cookie), None).await;
    assert_eq!(list.body[0]["last_used_at"].as_i64(), Some(first_used));

    let token_id = created["id"].as_str().expect("id").to_owned();
    app.state
        .db
        .call(move |connection| {
            connection.execute(
                "UPDATE tokens SET last_used_at = 1 WHERE id = ?1",
                [token_id],
            )?;
            Ok(())
        })
        .await
        .expect("age token");
    let _ = app.get("/api/me", None, Some(&token)).await;
    let list = app.get("/api/tokens", Some(&cookie), None).await;
    assert!(list.body[0]["last_used_at"].as_i64().unwrap_or(0) > 1);

    let id = created["id"].as_str().expect("id");
    assert_eq!(
        app.cookie_delete(&format!("/api/tokens/{id}"), &cookie)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.cookie_delete(&format!("/api/tokens/{id}"), &cookie)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    let response = app.get("/api/me", None, Some(&token)).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn agent_scope_matrix() {
    let app = TestApp::new();
    let cookie = app.login().await;
    let project = app.create_project(&cookie, "ALP").await;
    let project_id = project["id"].as_str().expect("project id");
    let label = app.create_label(&cookie, "bug", "#ff0000").await;
    let label_id = label["id"].as_str().expect("label id");
    let task = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "Card", "labels": [label_id]}),
        )
        .await;
    let reference = task["reference"].as_str().expect("reference");
    let checklist = app
        .cookie_post(
            &format!("/api/tasks/{reference}/checklist"),
            Some(json!({"text": "step"})),
            &cookie,
        )
        .await;
    assert_eq!(checklist.status, StatusCode::CREATED);
    let item_id = checklist.body["id"].as_str().expect("item id");

    let created = app.create_token(&cookie, "agent", "agent").await;
    let agent = created["token"].as_str().expect("token");
    let agent_token_id = created["id"].as_str().expect("id").to_owned();

    for (method, uri) in [
        (Method::GET, "/api/projects"),
        (Method::GET, "/api/tasks"),
        (Method::GET, "/api/labels"),
        (Method::GET, &format!("/api/tasks/{reference}")),
    ] {
        let response = app
            .request(method, uri, None, None, Some(agent), None)
            .await;
        assert_eq!(response.status, StatusCode::OK, "{uri}");
    }

    let denied = [
        (
            Method::POST,
            "/api/projects".to_owned(),
            Some(json!({"key": "BET", "name": "Beta"})),
        ),
        (
            Method::PATCH,
            format!("/api/projects/{project_id}"),
            Some(json!({"name": "Nope"})),
        ),
        (
            Method::POST,
            format!("/api/projects/{project_id}/archive"),
            None,
        ),
        (
            Method::POST,
            "/api/labels".to_owned(),
            Some(json!({"name": "nope", "color": "#000000"})),
        ),
        (
            Method::PATCH,
            format!("/api/labels/{label_id}"),
            Some(json!({"name": "nope"})),
        ),
        (Method::DELETE, format!("/api/labels/{label_id}"), None),
        (Method::GET, "/api/tokens".to_owned(), None),
        (
            Method::POST,
            "/api/tokens".to_owned(),
            Some(json!({"name": "nope", "scope": "agent"})),
        ),
        (
            Method::DELETE,
            format!("/api/tokens/{agent_token_id}"),
            None,
        ),
        (
            Method::PATCH,
            format!("/api/tasks/{reference}"),
            Some(json!({"title": "Nope"})),
        ),
        (
            Method::POST,
            format!("/api/tasks/{reference}/archive"),
            None,
        ),
        (
            Method::POST,
            format!("/api/tasks/{reference}/restore"),
            None,
        ),
        (Method::DELETE, format!("/api/tasks/{reference}"), None),
        (
            Method::DELETE,
            format!("/api/tasks/{reference}/checklist/{item_id}"),
            None,
        ),
    ];
    for (method, uri, body) in denied {
        let response = app
            .request(method, &uri, body, None, Some(agent), None)
            .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(response.code(), "forbidden", "{uri}");
    }

    let proposal = app
        .request(
            Method::POST,
            "/api/tasks",
            Some(json!({"project_key": "ALP", "title": "Agent idea", "status": "done"})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(proposal.status, StatusCode::CREATED, "{:?}", proposal.body);
    assert_eq!(proposal.body["state"], "proposed");
    assert_eq!(proposal.body["status"], "backlog");
    assert_eq!(proposal.body["source"], "agent");
    assert!(activity_kinds(&proposal.body).contains(&"proposed".to_owned()));

    let comment = app
        .request(
            Method::POST,
            &format!("/api/tasks/{reference}/comment"),
            Some(json!({"body": "working"})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(comment.status, StatusCode::CREATED);
    assert_eq!(comment.body["author"], "agent");

    let added = app
        .request(
            Method::POST,
            &format!("/api/tasks/{reference}/checklist"),
            Some(json!({"text": "agent step"})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(added.status, StatusCode::CREATED);
    let added_id = added.body["id"].as_str().expect("item id");
    let updated = app
        .request(
            Method::PATCH,
            &format!("/api/tasks/{reference}/checklist/{added_id}"),
            Some(json!({"done": true, "text": "done"})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(updated.status, StatusCode::OK);
    assert_eq!(updated.body["done"], true);
    assert_eq!(updated.body["text"], "done");
}

#[tokio::test]
async fn projects_crud_and_archive_filter() {
    let app = TestApp::new();
    let cookie = app.login().await;

    let first = app.create_project(&cookie, "alp").await;
    assert_eq!(first["key"], "ALP");
    assert_eq!(first["position"], 1.0);
    let second = app.create_project(&cookie, "BET").await;
    assert_eq!(second["position"], 2.0);
    let second_id = second["id"].as_str().expect("id");
    let rename_conflict = app
        .cookie_patch(
            &format!("/api/projects/{second_id}"),
            json!({"key": "alp"}),
            &cookie,
        )
        .await;
    assert_eq!(rename_conflict.status, StatusCode::CONFLICT);

    let duplicate = app
        .cookie_post(
            "/api/projects",
            Some(json!({"key": "Alp", "name": "Duplicate"})),
            &cookie,
        )
        .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(duplicate.code(), "conflict");

    for key in ["A", "TOOLONG", "A-B", "A B"] {
        let response = app
            .cookie_post(
                "/api/projects",
                Some(json!({"key": key, "name": "Bad"})),
                &cookie,
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{key}");
    }

    let id = first["id"].as_str().expect("id");
    for uri in [
        "/api/projects/ALP",
        "/api/projects/alp",
        &format!("/api/projects/{id}"),
    ] {
        let response = app.get(uri, Some(&cookie), None).await;
        assert_eq!(response.status, StatusCode::OK, "{uri}");
        assert_eq!(response.body["id"], id);
    }

    let patched = app
        .cookie_patch(
            &format!("/api/projects/{id}"),
            json!({
                "key": "OMG",
                "name": "Omega",
                "description": "desc",
                "repo_path": "/repo",
                "color": "#ABCDEF",
                "agent": "build",
                "model": "provider/model",
                "base_branch": "main",
                "worktree": false,
                "prompt_template": "hi {{ref}}"
            }),
            &cookie,
        )
        .await;
    assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
    assert_eq!(patched.body["key"], "OMG");
    assert_eq!(patched.body["name"], "Omega");
    assert_eq!(patched.body["description"], "desc");
    assert_eq!(patched.body["color"], "#abcdef");
    assert_eq!(patched.body["agent"], "build");
    assert_eq!(patched.body["model"], "provider/model");
    assert_eq!(patched.body["base_branch"], "main");
    assert_eq!(patched.body["worktree"], false);
    assert_eq!(patched.body["repo_path"], "/repo");
    assert_eq!(patched.body["prompt_template"], "hi {{ref}}");

    let archived = app
        .cookie_post(&format!("/api/projects/{id}/archive"), None, &cookie)
        .await;
    assert_eq!(archived.status, StatusCode::OK);
    assert!(archived.body["archived_at"].is_number());

    let active = app.get("/api/projects", Some(&cookie), None).await;
    let active = active.body.as_array().expect("array");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0]["key"], "BET");
    let all = app
        .get("/api/projects?archived=1", Some(&cookie), None)
        .await;
    let all = all.body.as_array().expect("array");
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|project| project["key"] == "OMG"));
    assert!(all.iter().any(|project| project["key"] == "BET"));

    let unknown = app
        .cookie_patch(
            &format!("/api/projects/{id}"),
            json!({"name": "Should not apply", "nope": 1}),
            &cookie,
        )
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown.code(), "validation");
    let unchanged = app
        .get(&format!("/api/projects/{id}"), Some(&cookie), None)
        .await;
    assert_eq!(unchanged.body["name"], "Omega");
}

#[tokio::test]
async fn tasks_create_list_filters_and_refs() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    let bug = app.create_label(&cookie, "bug", "#ff0000").await;
    let bug_id = bug["id"].as_str().expect("label id");

    let first = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "First", "labels": [bug_id]}),
        )
        .await;
    assert_eq!(first["number"], 1);
    assert_eq!(first["reference"], "ALP-1");
    assert_eq!(first["rank"], 1.0);
    assert_eq!(first["status"], "backlog");
    assert_eq!(first["priority"], "normal");
    assert_eq!(first["source"], "human");
    assert_eq!(first["state"], "active");
    assert_eq!(first["labels"][0]["name"], "bug");

    let second = app
        .create_task(
            &cookie,
            json!({"project_id": first["project_id"], "title": "Second"}),
        )
        .await;
    assert_eq!(second["number"], 2);
    assert_eq!(second["rank"], 2.0);

    let third = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "Third", "status": "todo"}),
        )
        .await;
    assert_eq!(third["number"], 3);
    assert_eq!(third["rank"], 1.0);

    app.create_project(&cookie, "BET").await;
    let other = app
        .create_task(
            &cookie,
            json!({"project_key": "BET", "title": "Other", "status": "in_review"}),
        )
        .await;
    assert_eq!(other["reference"], "BET-1");

    let uuid = first["id"].as_str().expect("id").to_owned();
    let refs = vec![
        "/api/tasks/ALP-1".to_owned(),
        "/api/tasks/alp-1".to_owned(),
        format!("/api/tasks/{uuid}"),
    ];
    for uri in refs {
        let response = app.get(&uri, Some(&cookie), None).await;
        assert_eq!(response.status, StatusCode::OK, "{uri}");
        assert_eq!(response.body["id"], uuid);
        assert!(response.body["checklist"].is_array());
        assert!(response.body["comments"].is_array());
        assert!(response.body["activity"].is_array());
        assert!(response.body["dependencies"]["blocked_by"].is_array());
    }

    let filtered = app
        .get("/api/tasks?project=alp&status=todo", Some(&cookie), None)
        .await;
    assert_eq!(filtered.body.as_array().expect("array").len(), 1);
    assert_eq!(filtered.body[0]["reference"], "ALP-3");

    let filtered = app.get("/api/tasks?label=BUG", Some(&cookie), None).await;
    assert_eq!(filtered.body.as_array().expect("array").len(), 1);
    assert_eq!(filtered.body[0]["reference"], "ALP-1");

    let filtered = app.get("/api/tasks?q=Third", Some(&cookie), None).await;
    assert_eq!(filtered.body.as_array().expect("array").len(), 1);

    let first_id = first["id"].as_str().expect("id").to_owned();
    let second_id = second["id"].as_str().expect("id").to_owned();
    let third_id = third["id"].as_str().expect("id").to_owned();
    app.state
        .db
        .call(move |connection| {
            connection.execute(
                "UPDATE tasks SET claimed_by = 'agent-x', attention = 1 WHERE id = ?1",
                [first_id],
            )?;
            connection.execute(
                "UPDATE tasks SET claimed_by = 'agent-x', attention = 0 WHERE id = ?1",
                [second_id],
            )?;
            connection.execute(
                "UPDATE tasks SET claimed_by = 'agent-y', attention = 1 WHERE id = ?1",
                [third_id],
            )?;
            Ok(())
        })
        .await
        .expect("mark tasks");
    let filtered = app
        .get("/api/tasks?claimed_by=agent-x", Some(&cookie), None)
        .await;
    let refs: Vec<&str> = filtered
        .body
        .as_array()
        .expect("array")
        .iter()
        .map(|task| task["reference"].as_str().expect("reference"))
        .collect();
    assert_eq!(refs, ["ALP-1", "ALP-2"]);
    let filtered = app.get("/api/tasks?attention=1", Some(&cookie), None).await;
    let refs: Vec<&str> = filtered
        .body
        .as_array()
        .expect("array")
        .iter()
        .map(|task| task["reference"].as_str().expect("reference"))
        .collect();
    assert_eq!(refs, ["ALP-1", "ALP-3"]);
    let filtered = app
        .get(
            "/api/tasks?claimed_by=agent-x&attention=1",
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(filtered.body.as_array().expect("array").len(), 1);
    assert_eq!(filtered.body[0]["reference"], "ALP-1");

    let page = app
        .get("/api/tasks?limit=1&offset=1", Some(&cookie), None)
        .await;
    let page = page.body.as_array().expect("array");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0]["reference"], "ALP-2");

    let other_ref = other["reference"].as_str().expect("reference");
    let archived = app
        .cookie_post(&format!("/api/tasks/{other_ref}/archive"), None, &cookie)
        .await;
    assert_eq!(archived.status, StatusCode::OK);

    let filtered = app
        .get("/api/tasks?state=active", Some(&cookie), None)
        .await;
    assert_eq!(filtered.body.as_array().expect("array").len(), 3);
    let filtered = app
        .get("/api/tasks?state=archived", Some(&cookie), None)
        .await;
    let filtered = filtered.body.as_array().expect("array");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0]["reference"], "BET-1");
    let default_list = app.get("/api/tasks", Some(&cookie), None).await;
    assert_eq!(default_list.body.as_array().expect("array").len(), 3);
    let project_id = first["project_id"].as_str().expect("project id");
    let filtered = app
        .get(
            &format!("/api/tasks?project={project_id}"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(filtered.body.as_array().expect("array").len(), 3);
    let filtered = app.get("/api/tasks?project=BET", Some(&cookie), None).await;
    assert!(filtered.body.as_array().expect("array").is_empty());
    let filtered = app
        .get("/api/tasks?project=BET&state=archived", Some(&cookie), None)
        .await;
    assert_eq!(filtered.body.as_array().expect("array").len(), 1);

    let bulk_project = project_id.to_owned();
    app.state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            for index in 0..501 {
                transaction.execute(
                    "INSERT INTO tasks
                     (id, project_id, number, title, status, state, rank, priority, source,
                      created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 'backlog', 'active', ?5, 'normal', 'human', 0, 0)",
                    rusqlite::params![
                        format!("bulk-{index}"),
                        bulk_project,
                        100 + index,
                        format!("Bulk {index}"),
                        (100 + index) as f64
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
        .expect("insert bulk tasks");

    let clamped = app.get("/api/tasks?limit=1000", Some(&cookie), None).await;
    assert_eq!(clamped.body.as_array().expect("array").len(), 500);

    let response = app
        .cookie_post(
            "/api/tasks",
            Some(json!({"project_key": "ALP", "title": "  "})),
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.code(), "validation");

    let response = app
        .cookie_post(
            "/api/tasks",
            Some(json!({"project_key": "ALP", "title": "Big", "description": "x".repeat(64 * 1024 + 1)})),
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    let response = app
        .cookie_post(
            "/api/tasks",
            Some(json!({"project_key": "NOPE", "title": "Missing"})),
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);

    let response = app
        .cookie_post(
            "/api/tasks",
            Some(json!({"project_key": "ALP", "title": "Bad label", "labels": ["missing"]})),
            &cookie,
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn task_patch_archive_restore_and_hard_delete_cascade() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    let label = app.create_label(&cookie, "bug", "#ff0000").await;
    let label_id = label["id"].as_str().expect("label id");
    let task = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "Original", "labels": [label_id]}),
        )
        .await;
    let reference = task["reference"].as_str().expect("reference");
    let id = task["id"].as_str().expect("id").to_owned();

    let patched = app
        .cookie_patch(
            &format!("/api/tasks/{reference}"),
            json!({
                "title": "Renamed",
                "description": "Body",
                "priority": "high",
                "due_date": "2026-12-01",
                "labels": []
            }),
            &cookie,
        )
        .await;
    assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
    assert_eq!(patched.body["title"], "Renamed");
    assert_eq!(patched.body["description"], "Body");
    assert_eq!(patched.body["priority"], "high");
    assert_eq!(patched.body["due_date"], "2026-12-01");
    assert!(patched.body["labels"]
        .as_array()
        .expect("labels")
        .is_empty());
    let kinds = activity_kinds(&patched.body);
    assert!(kinds.contains(&"updated".to_owned()));
    assert!(kinds.contains(&"label_removed".to_owned()));

    let cleared = app
        .cookie_patch(
            &format!("/api/tasks/{reference}"),
            json!({"due_date": null}),
            &cookie,
        )
        .await;
    assert_eq!(cleared.status, StatusCode::OK);
    assert!(cleared.body["due_date"].is_null());

    for forbidden in [
        json!({"title": "Should not apply", "status": "done"}),
        json!({"title": "Should not apply", "rank": 99.0}),
        json!({"title": "Should not apply", "project_id": "x"}),
    ] {
        let response = app
            .cookie_patch(&format!("/api/tasks/{reference}"), forbidden, &cookie)
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert_eq!(response.code(), "validation");
        let unchanged = app
            .get(&format!("/api/tasks/{reference}"), Some(&cookie), None)
            .await;
        assert_eq!(unchanged.body["title"], "Renamed");
    }

    let archived = app
        .cookie_post(&format!("/api/tasks/{reference}/archive"), None, &cookie)
        .await;
    assert_eq!(archived.status, StatusCode::OK);
    assert_eq!(archived.body["state"], "archived");
    assert!(archived.body["archived_at"].is_number());
    assert!(activity_kinds(&archived.body).contains(&"archived".to_owned()));

    let restored = app
        .cookie_post(&format!("/api/tasks/{reference}/restore"), None, &cookie)
        .await;
    assert_eq!(restored.status, StatusCode::OK);
    assert_eq!(restored.body["state"], "active");
    assert!(restored.body["archived_at"].is_null());
    assert!(activity_kinds(&restored.body).contains(&"restored".to_owned()));

    let relabelled = app
        .cookie_patch(
            &format!("/api/tasks/{reference}"),
            json!({"labels": [label_id]}),
            &cookie,
        )
        .await;
    assert_eq!(relabelled.status, StatusCode::OK, "{:?}", relabelled.body);
    assert_eq!(relabelled.body["labels"][0]["id"], label_id);

    // Add children and a dependency, then hard delete and verify the cascade.
    let item = app
        .cookie_post(
            &format!("/api/tasks/{reference}/checklist"),
            Some(json!({"text": "item"})),
            &cookie,
        )
        .await;
    assert_eq!(item.status, StatusCode::CREATED, "{:?}", item.body);
    let comment = app
        .cookie_post(
            &format!("/api/tasks/{reference}/comment"),
            Some(json!({"body": "note"})),
            &cookie,
        )
        .await;
    assert_eq!(comment.status, StatusCode::CREATED, "{:?}", comment.body);
    assert_eq!(comment.body["body"], "note");
    let dependency_task = app
        .create_task(&cookie, json!({"project_key": "ALP", "title": "Depends"}))
        .await;
    let dependency_id = dependency_task["id"].as_str().expect("id").to_owned();
    let blocked_by = id.clone();
    app.state
        .db
        .call(move |connection| {
            connection.execute(
                "INSERT INTO dependencies (task_id, blocked_by) VALUES (?1, ?2)",
                [dependency_id, blocked_by],
            )?;
            Ok(())
        })
        .await
        .expect("insert dependency");

    let no_hard = app
        .cookie_delete(&format!("/api/tasks/{reference}"), &cookie)
        .await;
    assert_eq!(no_hard.status, StatusCode::BAD_REQUEST);

    let deleted = app
        .cookie_delete(&format!("/api/tasks/{reference}?hard=true"), &cookie)
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let missing = app
        .get(&format!("/api/tasks/{reference}"), Some(&cookie), None)
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    let deleted_id = id.clone();
    let counts = app
        .state
        .db
        .call(move |connection| {
            let count = |table: &str| -> rusqlite::Result<i64> {
                connection.query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE task_id = ?1"),
                    [&deleted_id],
                    |row| row.get(0),
                )
            };
            Ok((
                count("checklist")?,
                count("comments")?,
                count("activity")?,
                count("task_labels")?,
            ))
        })
        .await
        .expect("cascade counts");
    assert_eq!(counts, (0, 0, 0, 0));
    assert_eq!(
        app.db_i64("SELECT COUNT(*) FROM dependencies", vec![])
            .await,
        0
    );
}

#[tokio::test]
async fn labels_crud_and_detach() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    let label = app.create_label(&cookie, "bug", "#FF0000").await;
    assert_eq!(label["color"], "#ff0000");
    let id = label["id"].as_str().expect("id");

    let duplicate = app
        .cookie_post(
            "/api/labels",
            Some(json!({"name": "BUG", "color": "#00ff00"})),
            &cookie,
        )
        .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);

    let invalid = app
        .cookie_post(
            "/api/labels",
            Some(json!({"name": "bad", "color": "red"})),
            &cookie,
        )
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);

    let second = app.create_label(&cookie, "feature", "#00ff00").await;
    let second_id = second["id"].as_str().expect("id");
    let rename_conflict = app
        .cookie_patch(
            &format!("/api/labels/{second_id}"),
            json!({"name": "BUG"}),
            &cookie,
        )
        .await;
    assert_eq!(rename_conflict.status, StatusCode::CONFLICT);

    let patched = app
        .cookie_patch(
            &format!("/api/labels/{id}"),
            json!({"name": "defect", "color": "#123456"}),
            &cookie,
        )
        .await;
    assert_eq!(patched.status, StatusCode::OK);
    assert_eq!(patched.body["name"], "defect");
    assert_eq!(patched.body["color"], "#123456");

    let task = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "Labelled", "labels": [id]}),
        )
        .await;
    let reference = task["reference"].as_str().expect("reference");

    let deleted = app
        .cookie_delete(&format!("/api/labels/{id}"), &cookie)
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let detail = app
        .get(&format!("/api/tasks/{reference}"), Some(&cookie), None)
        .await;
    assert!(detail.body["labels"].as_array().expect("labels").is_empty());
    assert!(activity_kinds(&detail.body).contains(&"label_removed".to_owned()));

    let missing = app
        .cookie_delete(&format!("/api/labels/{id}"), &cookie)
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn checklist_ordering_and_comment_attribution() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    let task = app
        .create_task(&cookie, json!({"project_key": "ALP", "title": "Steps"}))
        .await;
    let reference = task["reference"].as_str().expect("reference");

    let mut items = Vec::new();
    for text in ["one", "two", "three"] {
        let item = app
            .cookie_post(
                &format!("/api/tasks/{reference}/checklist"),
                Some(json!({"text": text})),
                &cookie,
            )
            .await;
        assert_eq!(item.status, StatusCode::CREATED);
        items.push(item.body);
    }
    let one = items[0]["id"].as_str().expect("id").to_owned();
    let two = items[1]["id"].as_str().expect("id").to_owned();
    let three = items[2]["id"].as_str().expect("id").to_owned();

    let ordered = app
        .get(&format!("/api/tasks/{reference}"), Some(&cookie), None)
        .await;
    let ids: Vec<&str> = ordered.body["checklist"]
        .as_array()
        .expect("checklist")
        .iter()
        .map(|item| item["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, [one.as_str(), two.as_str(), three.as_str()]);

    app.cookie_patch(
        &format!("/api/tasks/{reference}/checklist/{three}"),
        json!({"before": one}),
        &cookie,
    )
    .await;
    let ordered = app
        .get(&format!("/api/tasks/{reference}"), Some(&cookie), None)
        .await;
    let ids: Vec<&str> = ordered.body["checklist"]
        .as_array()
        .expect("checklist")
        .iter()
        .map(|item| item["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, [three.as_str(), one.as_str(), two.as_str()]);

    app.cookie_patch(
        &format!("/api/tasks/{reference}/checklist/{three}"),
        json!({"after": two}),
        &cookie,
    )
    .await;
    let ordered = app
        .get(&format!("/api/tasks/{reference}"), Some(&cookie), None)
        .await;
    let ids: Vec<&str> = ordered.body["checklist"]
        .as_array()
        .expect("checklist")
        .iter()
        .map(|item| item["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, [one.as_str(), two.as_str(), three.as_str()]);

    app.cookie_patch(
        &format!("/api/tasks/{reference}/checklist/{three}"),
        json!({"after": one, "before": two}),
        &cookie,
    )
    .await;
    let ordered = app
        .get(&format!("/api/tasks/{reference}"), Some(&cookie), None)
        .await;
    let ids: Vec<&str> = ordered.body["checklist"]
        .as_array()
        .expect("checklist")
        .iter()
        .map(|item| item["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, [one.as_str(), three.as_str(), two.as_str()]);

    // Seed neighbours below 2 * MIN_GAP so the next midpoint forces a renumber.
    let (one_seed, two_seed, three_seed) = (one.clone(), two.clone(), three.clone());
    app.state
        .db
        .call(move |connection| {
            connection.execute(
                "UPDATE checklist SET position = ?1 WHERE id = ?2",
                rusqlite::params![1.0_f64, one_seed],
            )?;
            connection.execute(
                "UPDATE checklist SET position = ?1 WHERE id = ?2",
                rusqlite::params![1.0_f64 + 1e-9, two_seed],
            )?;
            connection.execute(
                "UPDATE checklist SET position = ?1 WHERE id = ?2",
                rusqlite::params![2.0_f64, three_seed],
            )?;
            Ok(())
        })
        .await
        .expect("seed tight ranks");
    let renumbered = app
        .cookie_patch(
            &format!("/api/tasks/{reference}/checklist/{three}"),
            json!({"after": one, "before": two}),
            &cookie,
        )
        .await;
    assert_eq!(renumbered.status, StatusCode::OK, "{:?}", renumbered.body);

    let ordered = app
        .get(&format!("/api/tasks/{reference}"), Some(&cookie), None)
        .await;
    let checklist = ordered.body["checklist"].as_array().expect("checklist");
    let ids: Vec<&str> = checklist
        .iter()
        .map(|item| item["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, [one.as_str(), three.as_str(), two.as_str()]);
    let positions: Vec<f64> = checklist
        .iter()
        .map(|item| item["position"].as_f64().expect("position"))
        .collect();
    assert_eq!(positions, [1.0, 1.5, 2.0]);

    let invalid = app
        .cookie_patch(
            &format!("/api/tasks/{reference}/checklist/{three}"),
            json!({"after": two, "before": one}),
            &cookie,
        )
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);

    let updated = app
        .cookie_patch(
            &format!("/api/tasks/{reference}/checklist/{three}"),
            json!({"text": "THREE", "done": true}),
            &cookie,
        )
        .await;
    assert_eq!(updated.status, StatusCode::OK);
    assert_eq!(updated.body["text"], "THREE");
    assert_eq!(updated.body["done"], true);

    let comment = app
        .cookie_post(
            &format!("/api/tasks/{reference}/comment"),
            Some(json!({"body": "from ui"})),
            &cookie,
        )
        .await;
    assert_eq!(comment.status, StatusCode::CREATED);
    assert_eq!(comment.body["author"], "ui");

    let agent = app.create_token(&cookie, "agent", "agent").await;
    let agent = agent["token"].as_str().expect("token");
    let proposal = app
        .request(
            Method::POST,
            "/api/tasks",
            Some(json!({"project_key": "ALP", "title": "Agent idea", "status": "done"})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(proposal.status, StatusCode::CREATED, "{:?}", proposal.body);
    assert_eq!(proposal.body["state"], "proposed");
    assert_eq!(proposal.body["status"], "backlog");
    assert_eq!(proposal.body["source"], "agent");
    assert!(activity_kinds(&proposal.body).contains(&"proposed".to_owned()));

    let comment = app
        .request(
            Method::POST,
            &format!("/api/tasks/{reference}/comment"),
            Some(json!({"body": "from agent"})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(comment.status, StatusCode::CREATED);
    assert_eq!(comment.body["author"], "agent");

    let too_long = app
        .cookie_post(
            &format!("/api/tasks/{reference}/comment"),
            Some(json!({"body": "x".repeat(16 * 1024 + 1)})),
            &cookie,
        )
        .await;
    assert_eq!(too_long.status, StatusCode::BAD_REQUEST);

    let deleted = app
        .cookie_delete(&format!("/api/tasks/{reference}/checklist/{two}"), &cookie)
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let ordered = app
        .get(&format!("/api/tasks/{reference}"), Some(&cookie), None)
        .await;
    assert_eq!(
        ordered.body["checklist"]
            .as_array()
            .expect("checklist")
            .len(),
        2
    );
}

#[tokio::test]
async fn every_task_mutation_writes_activity() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    let first = app.create_label(&cookie, "one", "#111111").await;
    let first = first["id"].as_str().expect("id");
    let task = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "Activity", "labels": [first]}),
        )
        .await;
    let reference = task["reference"].as_str().expect("reference").to_owned();
    assert!(activity_kinds(&task).contains(&"created".to_owned()));
    assert!(activity_kinds(&task).contains(&"label_added".to_owned()));

    app.cookie_patch(
        &format!("/api/tasks/{reference}"),
        json!({"title": "Changed"}),
        &cookie,
    )
    .await;
    let second = app.create_label(&cookie, "two", "#222222").await;
    let second_id = second["id"].as_str().expect("id");
    app.cookie_patch(
        &format!("/api/tasks/{reference}"),
        json!({"labels": [first, second_id]}),
        &cookie,
    )
    .await;
    app.cookie_patch(
        &format!("/api/tasks/{reference}"),
        json!({"labels": [first]}),
        &cookie,
    )
    .await;

    app.cookie_post(
        &format!("/api/tasks/{reference}/comment"),
        Some(json!({"body": "hi"})),
        &cookie,
    )
    .await;
    let item = app
        .cookie_post(
            &format!("/api/tasks/{reference}/checklist"),
            Some(json!({"text": "step"})),
            &cookie,
        )
        .await;
    let item_id = item.body["id"].as_str().expect("id").to_owned();
    app.cookie_patch(
        &format!("/api/tasks/{reference}/checklist/{item_id}"),
        json!({"done": true}),
        &cookie,
    )
    .await;
    app.cookie_delete(
        &format!("/api/tasks/{reference}/checklist/{item_id}"),
        &cookie,
    )
    .await;
    app.cookie_post(&format!("/api/tasks/{reference}/archive"), None, &cookie)
        .await;
    app.cookie_post(&format!("/api/tasks/{reference}/restore"), None, &cookie)
        .await;

    let detail = app
        .get(&format!("/api/tasks/{reference}"), Some(&cookie), None)
        .await;
    let kinds = activity_kinds(&detail.body);
    for expected in [
        "created",
        "label_added",
        "updated",
        "label_removed",
        "commented",
        "checklist_added",
        "checklist_updated",
        "checklist_removed",
        "archived",
        "restored",
    ] {
        assert!(
            kinds.iter().any(|kind| kind == expected),
            "missing {expected}: {kinds:?}"
        );
    }
    let comment = detail.body["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .find(|entry| entry["kind"] == "commented")
        .expect("commented activity");
    assert!(comment["data"]["comment_id"].is_string());
}

#[tokio::test]
async fn validation_limits_and_error_envelope() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;

    let response = app
        .request(
            Method::POST,
            "/api/projects",
            Some(json!({"key": "BET", "name": "x".repeat(121)})),
            Some(&cookie),
            None,
            Some(ORIGIN),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.body["error"]["code"], "validation");
    assert!(response.body["error"]["message"].is_string());

    let bad_json = app
        .request(
            Method::POST,
            "/api/projects",
            Some(json!({"key": 1, "name": "Bad type"})),
            Some(&cookie),
            None,
            Some(ORIGIN),
        )
        .await;
    assert_eq!(bad_json.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_json.code(), "validation");

    let task = app
        .create_task(&cookie, json!({"project_key": "ALP", "title": "Limits"}))
        .await;
    let reference = task["reference"].as_str().expect("reference");

    let too_long = app
        .cookie_patch(
            &format!("/api/tasks/{reference}"),
            json!({"description": "x".repeat(64 * 1024 + 1)}),
            &cookie,
        )
        .await;
    assert_eq!(too_long.status, StatusCode::BAD_REQUEST);

    let bad_date = app
        .cookie_patch(
            &format!("/api/tasks/{reference}"),
            json!({"due_date": "2026-02-30"}),
            &cookie,
        )
        .await;
    assert_eq!(bad_date.status, StatusCode::BAD_REQUEST);

    let long_label = app
        .cookie_post(
            "/api/labels",
            Some(json!({"name": "x".repeat(33), "color": "#000000"})),
            &cookie,
        )
        .await;
    assert_eq!(long_label.status, StatusCode::BAD_REQUEST);

    let long_item = app
        .cookie_post(
            &format!("/api/tasks/{reference}/checklist"),
            Some(json!({"text": "x".repeat(501)})),
            &cookie,
        )
        .await;
    assert_eq!(long_item.status, StatusCode::BAD_REQUEST);

    let missing = app.get("/api/tasks/ALP-999", Some(&cookie), None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.code(), "not_found");

    let unauthorized = app.get("/api/me", None, None).await;
    assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);
    assert_eq!(unauthorized.code(), "unauthorized");
}

fn task_refs(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("task array")
        .iter()
        .map(|task| task["reference"].as_str().expect("reference").to_owned())
        .collect()
}

#[tokio::test]
async fn move_rank_top_bottom_between_and_renumber() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    let first = app
        .create_task(&cookie, json!({"project_key": "ALP", "title": "First"}))
        .await;
    let second = app
        .create_task(&cookie, json!({"project_key": "ALP", "title": "Second"}))
        .await;
    let third = app
        .create_task(&cookie, json!({"project_key": "ALP", "title": "Third"}))
        .await;
    let first_id = first["id"].as_str().expect("id");
    let second_id = second["id"].as_str().expect("id");
    let third_id = third["id"].as_str().expect("id");

    let moved = app
        .cookie_post(
            "/api/tasks/ALP-3/move",
            Some(json!({"status": "backlog", "before": "ALP-1"})),
            &cookie,
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);
    let list = app
        .get("/api/tasks?status=backlog", Some(&cookie), None)
        .await;
    assert_eq!(task_refs(&list.body), ["ALP-3", "ALP-1", "ALP-2"]);
    assert!(activity_kinds(&moved.body).contains(&"moved".to_owned()));

    app.cookie_post(
        "/api/tasks/ALP-3/move",
        Some(json!({"status": "backlog"})),
        &cookie,
    )
    .await;
    let list = app
        .get("/api/tasks?status=backlog", Some(&cookie), None)
        .await;
    assert_eq!(task_refs(&list.body), ["ALP-1", "ALP-2", "ALP-3"]);

    app.cookie_post(
        "/api/tasks/ALP-3/move",
        Some(json!({"status": "backlog", "after": "ALP-1", "before": "ALP-2"})),
        &cookie,
    )
    .await;
    let list = app
        .get("/api/tasks?status=backlog", Some(&cookie), None)
        .await;
    assert_eq!(task_refs(&list.body), ["ALP-1", "ALP-3", "ALP-2"]);

    let reversed = app
        .cookie_post(
            "/api/tasks/ALP-3/move",
            Some(json!({"status": "backlog", "after": "ALP-2", "before": "ALP-1"})),
            &cookie,
        )
        .await;
    assert_eq!(reversed.status, StatusCode::BAD_REQUEST);
    assert_eq!(reversed.code(), "validation");

    let non_adjacent = app
        .cookie_post(
            "/api/tasks/ALP-3/move",
            Some(json!({"status": "backlog", "after": "ALP-1", "before": "ALP-2"})),
            &cookie,
        )
        .await;
    assert_eq!(non_adjacent.status, StatusCode::OK);

    let cross = app
        .cookie_post(
            "/api/tasks/ALP-3/move",
            Some(json!({"status": "todo"})),
            &cookie,
        )
        .await;
    assert_eq!(cross.status, StatusCode::OK);
    assert_eq!(cross.body["status"], "todo");

    let done = app
        .cookie_post(
            "/api/tasks/ALP-3/move",
            Some(json!({"status": "done"})),
            &cookie,
        )
        .await;
    assert_eq!(done.status, StatusCode::OK);
    assert!(done.body["done_at"].is_number());
    let reopened = app
        .cookie_post(
            "/api/tasks/ALP-3/move",
            Some(json!({"status": "todo"})),
            &cookie,
        )
        .await;
    assert_eq!(reopened.status, StatusCode::OK);
    assert!(reopened.body["done_at"].is_null());

    // Seed a gap below 2 * MIN_GAP and insert a fourth card between A and B.
    let fourth = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "Fourth", "status": "todo"}),
        )
        .await;
    let fourth_ref = fourth["reference"].as_str().expect("reference");
    let (first_seed, second_seed) = (first_id.to_owned(), second_id.to_owned());
    app.state
        .db
        .call(move |connection| {
            connection.execute("UPDATE tasks SET rank = 1.0 WHERE id = ?1", [first_seed])?;
            connection.execute(
                "UPDATE tasks SET rank = ?1 WHERE id = ?2",
                rusqlite::params![1.0_f64 + 1e-9, second_seed],
            )?;
            Ok(())
        })
        .await
        .expect("seed tight ranks");
    let renumbered = app
        .cookie_post(
            &format!("/api/tasks/{fourth_ref}/move"),
            Some(json!({"status": "backlog", "after": "ALP-1", "before": "ALP-2"})),
            &cookie,
        )
        .await;
    assert_eq!(renumbered.status, StatusCode::OK, "{:?}", renumbered.body);
    let list = app
        .get("/api/tasks?status=backlog", Some(&cookie), None)
        .await;
    assert_eq!(task_refs(&list.body), ["ALP-1", fourth_ref, "ALP-2"]);
    let ranks: Vec<f64> = list
        .body
        .as_array()
        .expect("array")
        .iter()
        .map(|task| task["rank"].as_f64().expect("rank"))
        .collect();
    assert_eq!(ranks, [1.0, 2.0, 3.0]);
    let _ = third_id;
}

#[tokio::test]
async fn blocker_round_trip_restore_and_explicit_status() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Blocked", "status": "todo"}),
    )
    .await;

    let blocked = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"status": "blocker"})),
            &cookie,
        )
        .await;
    assert_eq!(blocked.status, StatusCode::OK, "{:?}", blocked.body);
    assert_eq!(blocked.body["status"], "blocker");
    assert_eq!(blocked.body["blocked_from"], "todo");
    assert!(activity_kinds(&blocked.body).contains(&"blocked".to_owned()));

    let reordered = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"status": "blocker"})),
            &cookie,
        )
        .await;
    assert_eq!(reordered.status, StatusCode::OK);
    assert_eq!(reordered.body["blocked_from"], "todo");

    let restored = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"restore": true})),
            &cookie,
        )
        .await;
    assert_eq!(restored.status, StatusCode::OK);
    assert_eq!(restored.body["status"], "todo");
    assert!(restored.body["blocked_from"].is_null());
    assert!(activity_kinds(&restored.body).contains(&"unblocked".to_owned()));

    app.cookie_post(
        "/api/tasks/ALP-1/move",
        Some(json!({"status": "blocker"})),
        &cookie,
    )
    .await;
    let explicit = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"status": "in_review", "restore": true})),
            &cookie,
        )
        .await;
    assert_eq!(explicit.status, StatusCode::OK);
    assert_eq!(explicit.body["status"], "in_review");
    assert!(explicit.body["blocked_from"].is_null());

    app.cookie_post(
        "/api/tasks/ALP-1/move",
        Some(json!({"status": "blocker"})),
        &cookie,
    )
    .await;
    let cleared = app
        .cookie_post("/api/tasks/ALP-1/move", Some(json!({})), &cookie)
        .await;
    assert_eq!(cleared.status, StatusCode::OK);
    assert_eq!(cleared.body["status"], "blocker");
    assert!(cleared.body["blocked_from"].is_null());
    assert!(cleared.body["blocked_reason"].is_null());
    assert!(activity_kinds(&cleared.body).contains(&"unblocked".to_owned()));

    let invalid = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"restore": true})),
            &cookie,
        )
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.code(), "validation");
}

#[tokio::test]
async fn claim_release_conflicts_and_agent_scope() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    let agent = app.create_token(&cookie, "agent", "agent").await;
    let agent = agent["token"].as_str().expect("token");
    let other = app.create_token(&cookie, "other", "agent").await;
    let other = other["token"].as_str().expect("token");

    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Claimable", "status": "todo"}),
    )
    .await;
    let claimed = app
        .request(
            Method::POST,
            "/api/tasks/ALP-1/claim",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(claimed.status, StatusCode::OK, "{:?}", claimed.body);
    assert_eq!(claimed.body["status"], "in_progress");
    assert_eq!(claimed.body["claimed_by"], "agent");
    assert!(activity_kinds(&claimed.body).contains(&"claimed".to_owned()));

    let again = app
        .request(
            Method::POST,
            "/api/tasks/ALP-1/claim",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.code(), "already_claimed");

    let released = app
        .request(
            Method::POST,
            "/api/tasks/ALP-1/release",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(released.status, StatusCode::OK, "{:?}", released.body);
    assert!(released.body["claimed_by"].is_null());
    assert_eq!(released.body["status"], "in_progress");
    assert!(activity_kinds(&released.body).contains(&"released".to_owned()));

    let unclaimed = app
        .request(
            Method::POST,
            "/api/tasks/ALP-1/release",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(unclaimed.status, StatusCode::CONFLICT);
    assert_eq!(unclaimed.code(), "not_claimed");

    app.cookie_post(
        "/api/tasks/ALP-1/move",
        Some(json!({"status": "todo"})),
        &cookie,
    )
    .await;
    let reclaimed = app
        .request(
            Method::POST,
            "/api/tasks/ALP-1/claim",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(reclaimed.status, StatusCode::OK, "{:?}", reclaimed.body);
    let other_release = app
        .request(
            Method::POST,
            "/api/tasks/ALP-1/release",
            None,
            None,
            Some(other),
            None,
        )
        .await;
    assert_eq!(other_release.status, StatusCode::FORBIDDEN);
    assert_eq!(other_release.code(), "forbidden");
    let human_release = app
        .cookie_post("/api/tasks/ALP-1/release", None, &cookie)
        .await;
    assert_eq!(human_release.status, StatusCode::OK);

    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Review", "status": "in_review"}),
    )
    .await;
    let wrong_status = app
        .request(
            Method::POST,
            "/api/tasks/ALP-2/claim",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(wrong_status.status, StatusCode::CONFLICT);
    assert_eq!(wrong_status.code(), "not_claimable");

    // A dependency that is not done blocks the claim.
    let dependency = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "Dependency", "status": "todo"}),
        )
        .await;
    let dependency_id = dependency["id"].as_str().expect("id").to_owned();
    let blocked = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "Blocked", "status": "todo"}),
        )
        .await;
    let blocked_id = blocked["id"].as_str().expect("id").to_owned();
    let blocked_ref = blocked["reference"].as_str().expect("reference").to_owned();
    let blocked_for_insert = blocked_id.clone();
    app.state
        .db
        .call(move |connection| {
            connection.execute(
                "INSERT INTO dependencies (task_id, blocked_by) VALUES (?1, ?2)",
                [blocked_for_insert, dependency_id],
            )?;
            Ok(())
        })
        .await
        .expect("insert dependency");
    let unmet = app
        .request(
            Method::POST,
            &format!("/api/tasks/{blocked_ref}/claim"),
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(unmet.status, StatusCode::CONFLICT);
    assert_eq!(unmet.code(), "dependency_unmet");
    let dependency_ref = dependency["reference"].as_str().expect("reference");
    app.cookie_post(
        &format!("/api/tasks/{dependency_ref}/move"),
        Some(json!({"status": "done"})),
        &cookie,
    )
    .await;
    let now_claimable = app
        .request(
            Method::POST,
            &format!("/api/tasks/{blocked_ref}/claim"),
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(
        now_claimable.status,
        StatusCode::OK,
        "{:?}",
        now_claimable.body
    );

    // Concurrent claims leave exactly one winner.
    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Race", "status": "todo"}),
    )
    .await;
    let race_ref = "ALP-5";
    let race_uri = format!("/api/tasks/{race_ref}/claim");
    let (a, b, c, d) = tokio::join!(
        app.request(Method::POST, &race_uri, None, None, Some(agent), None),
        app.request(Method::POST, &race_uri, None, None, Some(agent), None),
        app.request(Method::POST, &race_uri, None, None, Some(other), None),
        app.request(Method::POST, &race_uri, None, None, Some(agent), None)
    );
    let responses = [a, b, c, d];
    assert_eq!(
        responses
            .iter()
            .filter(|response| response.status == StatusCode::OK)
            .count(),
        1
    );
    assert!(responses
        .iter()
        .filter(|response| response.status == StatusCode::CONFLICT)
        .all(|response| response.code() == "already_claimed"));

    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Human claim", "status": "todo"}),
    )
    .await;
    let human_claim = app
        .cookie_post("/api/tasks/ALP-6/claim", None, &cookie)
        .await;
    assert_eq!(human_claim.status, StatusCode::OK);
    assert_eq!(human_claim.body["claimed_by"], "ui");
}

#[tokio::test]
async fn agent_next_orders_and_claims() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    app.create_project(&cookie, "BET").await;
    let agent = app.create_token(&cookie, "agent", "agent").await;
    let agent = agent["token"].as_str().expect("token");

    let high_late = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "High late", "status": "backlog", "priority": "high", "due_date": "2026-01-02"}),
        )
        .await;
    let high_early = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "High early", "status": "backlog", "priority": "high", "due_date": "2026-01-01"}),
        )
        .await;
    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Normal none", "status": "backlog"}),
    )
    .await;
    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Normal dated", "status": "todo", "due_date": "2025-12-31"}),
    )
    .await;
    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Low", "status": "backlog", "priority": "low", "due_date": "2024-01-01"}),
    )
    .await;
    app.create_task(
        &cookie,
        json!({"project_key": "BET", "title": "Other project", "status": "backlog", "priority": "high", "due_date": "2026-01-01"}),
    )
    .await;

    let next = app
        .request(
            Method::GET,
            "/api/agent/next?project=ALP",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(next.status, StatusCode::OK, "{:?}", next.body);
    assert_eq!(next.body["reference"], high_early["reference"]);
    assert!(next.body["checklist"].is_array());
    assert!(next.body["dependencies"]["blocked_by"].is_array());

    let claimed = app
        .request(
            Method::GET,
            "/api/agent/next?project=ALP&claim=1",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(claimed.status, StatusCode::OK, "{:?}", claimed.body);
    assert_eq!(claimed.body["reference"], high_early["reference"]);
    assert_eq!(claimed.body["status"], "in_progress");
    assert_eq!(claimed.body["claimed_by"], "agent");

    let next = app
        .request(
            Method::GET,
            "/api/agent/next?project=ALP",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(next.body["reference"], high_late["reference"]);

    let global = app
        .request(
            Method::GET,
            "/api/agent/next",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(global.body["reference"], "BET-1");

    for _ in 0..4 {
        let claimed = app
            .request(
                Method::GET,
                "/api/agent/next?project=ALP&claim=1",
                None,
                None,
                Some(agent),
                None,
            )
            .await;
        assert_eq!(claimed.status, StatusCode::OK, "{:?}", claimed.body);
    }
    let empty = app
        .request(
            Method::GET,
            "/api/agent/next?project=ALP&claim=1",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(empty.status, StatusCode::NO_CONTENT);

    let unknown = app
        .request(
            Method::GET,
            "/api/agent/next?project=NOPE",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn agent_move_scope_is_limited_to_working_columns() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    let agent = app.create_token(&cookie, "agent", "agent").await;
    let agent = agent["token"].as_str().expect("token");

    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Working", "status": "todo"}),
    )
    .await;
    let move_to = |status: &'static str| {
        let app = &app;
        async move {
            app.request(
                Method::POST,
                "/api/tasks/ALP-1/move",
                Some(json!({"status": status})),
                None,
                Some(agent),
                None,
            )
            .await
        }
    };
    assert_eq!(move_to("in_progress").await.status, StatusCode::OK);
    assert_eq!(move_to("in_review").await.status, StatusCode::OK);
    assert_eq!(move_to("blocker").await.status, StatusCode::OK);
    let restored = app
        .request(
            Method::POST,
            "/api/tasks/ALP-1/move",
            Some(json!({"restore": true})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(restored.status, StatusCode::OK);
    assert_eq!(restored.body["status"], "in_review");

    for status in ["backlog", "done"] {
        let denied = move_to(status).await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{status}");
        assert_eq!(denied.code(), "forbidden");
    }

    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Backlog source", "status": "backlog"}),
    )
    .await;
    let from_backlog = app
        .request(
            Method::POST,
            "/api/tasks/ALP-2/move",
            Some(json!({"status": "todo"})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(from_backlog.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn next_claim_requires_csrf_for_cookie_requests() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Next", "status": "backlog"}),
    )
    .await;

    let denied = app
        .request(
            Method::GET,
            "/api/agent/next?claim=1",
            None,
            Some(&cookie),
            None,
            None,
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.code(), "csrf");

    let allowed = app
        .request(
            Method::GET,
            "/api/agent/next?claim=1",
            None,
            Some(&cookie),
            None,
            Some(ORIGIN),
        )
        .await;
    assert_eq!(allowed.status, StatusCode::OK, "{:?}", allowed.body);
    assert_eq!(allowed.body["claimed_by"], "ui");
}

#[tokio::test]
async fn ranks_survive_archive_move_and_restore() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    for title in ["First", "Second", "Third"] {
        app.create_task(&cookie, json!({"project_key": "ALP", "title": title}))
            .await;
    }

    app.cookie_post("/api/tasks/ALP-1/archive", None, &cookie)
        .await;
    let moved = app
        .cookie_post(
            "/api/tasks/ALP-3/move",
            Some(json!({"status": "backlog", "before": "ALP-2"})),
            &cookie,
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);
    app.cookie_post("/api/tasks/ALP-1/restore", None, &cookie)
        .await;

    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Fourth", "status": "backlog"}),
    )
    .await;
    let between = app
        .cookie_post(
            "/api/tasks/ALP-4/move",
            Some(json!({"status": "backlog", "after": "ALP-1", "before": "ALP-3"})),
            &cookie,
        )
        .await;
    assert_eq!(between.status, StatusCode::OK, "{:?}", between.body);
    let list = app
        .get("/api/tasks?status=backlog", Some(&cookie), None)
        .await;
    assert_eq!(task_refs(&list.body), ["ALP-1", "ALP-4", "ALP-3", "ALP-2"]);
}

#[tokio::test]
async fn token_named_ui_is_rejected() {
    let app = TestApp::new();
    let cookie = app.login().await;
    for name in ["ui", "UI", " Ui "] {
        let response = app
            .cookie_post(
                "/api/tokens",
                Some(json!({"name": name, "scope": "agent"})),
                &cookie,
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(response.code(), "validation");
    }
}

#[tokio::test]
async fn hidden_archived_card_does_not_block_visible_neighbours() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    for title in ["First", "Hidden", "Third"] {
        app.create_task(&cookie, json!({"project_key": "ALP", "title": title}))
            .await;
    }
    app.cookie_post("/api/tasks/ALP-2/archive", None, &cookie)
        .await;
    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Fourth", "status": "backlog"}),
    )
    .await;

    let moved = app
        .cookie_post(
            "/api/tasks/ALP-4/move",
            Some(json!({"status": "backlog", "after": "ALP-1", "before": "ALP-3"})),
            &cookie,
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);
    let list = app
        .get(
            "/api/tasks?status=backlog&state=active",
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(task_refs(&list.body), ["ALP-1", "ALP-4", "ALP-3"]);

    let restored = app
        .cookie_post("/api/tasks/ALP-2/restore", None, &cookie)
        .await;
    assert_eq!(restored.status, StatusCode::OK);
    let list = app
        .get(
            "/api/tasks?status=backlog&state=active",
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(task_refs(&list.body), ["ALP-1", "ALP-2", "ALP-4", "ALP-3"]);
}

#[tokio::test]
async fn dependencies_set_replace_clear_and_cycles() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    app.create_project(&cookie, "BET").await;
    for title in ["First", "Second", "Third"] {
        app.create_task(&cookie, json!({"project_key": "ALP", "title": title}))
            .await;
    }
    app.create_task(&cookie, json!({"project_key": "BET", "title": "Other"}))
        .await;

    let set = app
        .cookie_post(
            "/api/tasks/ALP-1/dependencies",
            Some(json!({"blocked_by": ["ALP-2", "ALP-3"]})),
            &cookie,
        )
        .await;
    assert_eq!(set.status, StatusCode::OK, "{:?}", set.body);
    let refs: Vec<&str> = set.body["dependencies"]["blocked_by"]
        .as_array()
        .expect("blocked_by")
        .iter()
        .map(|dependency| dependency["reference"].as_str().expect("reference"))
        .collect();
    assert_eq!(refs, ["ALP-2", "ALP-3"]);
    assert!(activity_kinds(&set.body).contains(&"dependencies_changed".to_owned()));

    let replaced = app
        .cookie_post(
            "/api/tasks/ALP-1/dependencies",
            Some(json!({"blocked_by": ["ALP-3"]})),
            &cookie,
        )
        .await;
    assert_eq!(replaced.status, StatusCode::OK);
    assert_eq!(
        replaced.body["dependencies"]["blocked_by"]
            .as_array()
            .expect("blocked_by")
            .len(),
        1
    );
    let cleared = app
        .cookie_post(
            "/api/tasks/ALP-1/dependencies",
            Some(json!({"blocked_by": []})),
            &cookie,
        )
        .await;
    assert_eq!(cleared.status, StatusCode::OK);
    assert!(cleared.body["dependencies"]["blocked_by"]
        .as_array()
        .expect("blocked_by")
        .is_empty());

    let self_dependency = app
        .cookie_post(
            "/api/tasks/ALP-1/dependencies",
            Some(json!({"blocked_by": ["ALP-1"]})),
            &cookie,
        )
        .await;
    assert_eq!(self_dependency.status, StatusCode::BAD_REQUEST);
    assert_eq!(self_dependency.code(), "validation");

    app.cookie_post(
        "/api/tasks/ALP-1/dependencies",
        Some(json!({"blocked_by": ["ALP-2"]})),
        &cookie,
    )
    .await;
    let cycle = app
        .cookie_post(
            "/api/tasks/ALP-2/dependencies",
            Some(json!({"blocked_by": ["ALP-1"]})),
            &cookie,
        )
        .await;
    assert_eq!(cycle.status, StatusCode::BAD_REQUEST);
    assert_eq!(cycle.code(), "validation");

    let unknown = app
        .cookie_post(
            "/api/tasks/ALP-1/dependencies",
            Some(json!({"blocked_by": ["ALP-999"]})),
            &cookie,
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.code(), "not_found");

    let cross_project = app
        .cookie_post(
            "/api/tasks/ALP-1/dependencies",
            Some(json!({"blocked_by": ["BET-1"]})),
            &cookie,
        )
        .await;
    assert_eq!(
        cross_project.status,
        StatusCode::OK,
        "{:?}",
        cross_project.body
    );
    assert_eq!(
        cross_project.body["dependencies"]["blocked_by"][0]["reference"],
        "BET-1"
    );

    // The dependency gate also works through the endpoint.
    app.cookie_post(
        "/api/tasks/ALP-1/dependencies",
        Some(json!({"blocked_by": ["ALP-2"]})),
        &cookie,
    )
    .await;
    let agent = app.create_token(&cookie, "agent", "agent").await;
    let agent = agent["token"].as_str().expect("token");
    let unmet = app
        .request(
            Method::POST,
            "/api/tasks/ALP-1/claim",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(unmet.status, StatusCode::CONFLICT);
    assert_eq!(unmet.code(), "dependency_unmet");
    app.cookie_post(
        "/api/tasks/ALP-2/move",
        Some(json!({"status": "done"})),
        &cookie,
    )
    .await;
    let claimable = app
        .request(
            Method::POST,
            "/api/tasks/ALP-1/claim",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(claimable.status, StatusCode::OK, "{:?}", claimable.body);

    let agent_dependency = app
        .request(
            Method::POST,
            "/api/tasks/ALP-3/dependencies",
            Some(json!({"blocked_by": []})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(agent_dependency.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn proposals_inbox_accept_and_reject() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    let agent = app.create_token(&cookie, "agent", "agent").await;
    let agent = agent["token"].as_str().expect("token");

    let first = app
        .request(
            Method::POST,
            "/api/tasks",
            Some(json!({"project_key": "ALP", "title": "First idea", "status": "done"})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "{:?}", first.body);
    assert_eq!(first.body["state"], "proposed");
    assert_eq!(first.body["status"], "backlog");
    assert_eq!(first.body["source"], "agent");
    assert!(activity_kinds(&first.body).contains(&"proposed".to_owned()));

    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let second = app
        .request(
            Method::POST,
            "/api/tasks",
            Some(json!({"project_key": "ALP", "title": "Second idea"})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(second.status, StatusCode::CREATED);

    let active = app
        .create_task(&cookie, json!({"project_key": "ALP", "title": "Active"}))
        .await;

    let default_list = app.get("/api/tasks", Some(&cookie), None).await;
    let refs = task_refs(&default_list.body);
    assert_eq!(refs, [active["reference"].as_str().expect("reference")]);
    assert!(!refs.contains(&"ALP-1".to_owned()));
    assert!(!refs.contains(&"ALP-2".to_owned()));

    let proposed = app
        .get("/api/tasks?state=proposed", Some(&cookie), None)
        .await;
    assert_eq!(proposed.body.as_array().expect("array").len(), 2);

    let board = app
        .get("/api/projects/ALP/board", Some(&cookie), None)
        .await;
    assert_eq!(
        board.body["columns"]["backlog"]
            .as_array()
            .expect("backlog")
            .len(),
        1
    );
    assert_eq!(
        board.body["columns"]["backlog"][0]["reference"],
        active["reference"]
    );

    let next = app
        .request(
            Method::GET,
            "/api/agent/next?project=ALP",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(next.body["reference"], active["reference"]);

    let inbox = app.get("/api/inbox", Some(&cookie), None).await;
    let inbox_refs = task_refs(&inbox.body);
    assert_eq!(inbox_refs, ["ALP-2", "ALP-1"]);

    let accepted = app
        .cookie_post(
            "/api/inbox/ALP-1/accept",
            Some(json!({"status": "todo"})),
            &cookie,
        )
        .await;
    assert_eq!(accepted.status, StatusCode::OK, "{:?}", accepted.body);
    assert_eq!(accepted.body["state"], "active");
    assert_eq!(accepted.body["status"], "todo");
    assert!(activity_kinds(&accepted.body).contains(&"accepted".to_owned()));

    let rejected = app
        .cookie_post("/api/inbox/ALP-2/reject", None, &cookie)
        .await;
    assert_eq!(rejected.status, StatusCode::OK, "{:?}", rejected.body);
    assert_eq!(rejected.body["state"], "archived");
    assert!(rejected.body["archived_at"].is_number());
    assert!(activity_kinds(&rejected.body).contains(&"rejected".to_owned()));

    let inbox = app.get("/api/inbox", Some(&cookie), None).await;
    assert!(inbox.body.as_array().expect("array").is_empty());

    let agent_accept = app
        .request(
            Method::POST,
            "/api/inbox/ALP-1/accept",
            Some(json!({})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(agent_accept.status, StatusCode::FORBIDDEN);
    let agent_reject = app
        .request(
            Method::POST,
            "/api/inbox/ALP-1/reject",
            None,
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(agent_reject.status, StatusCode::FORBIDDEN);

    let blocker_proposal = app
        .request(
            Method::POST,
            "/api/tasks",
            Some(json!({"project_key": "ALP", "title": "Blocked idea"})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(blocker_proposal.status, StatusCode::CREATED);
    let blocker_ref = blocker_proposal.body["reference"]
        .as_str()
        .expect("reference");
    let accepted_blocker = app
        .cookie_post(
            &format!("/api/inbox/{blocker_ref}/accept"),
            Some(json!({"status": "blocker"})),
            &cookie,
        )
        .await;
    assert_eq!(
        accepted_blocker.status,
        StatusCode::OK,
        "{:?}",
        accepted_blocker.body
    );
    assert_eq!(accepted_blocker.body["status"], "blocker");
    assert_eq!(accepted_blocker.body["blocked_from"], "backlog");
    let restored = app
        .cookie_post(
            &format!("/api/tasks/{blocker_ref}/move"),
            Some(json!({"restore": true})),
            &cookie,
        )
        .await;
    assert_eq!(restored.status, StatusCode::OK, "{:?}", restored.body);
    assert_eq!(restored.body["status"], "backlog");
}

#[tokio::test]
async fn board_grouping_ordering_isolation_and_global_view() {
    let app = TestApp::new();
    let cookie = app.login().await;
    let alpha = app.create_project(&cookie, "ALP").await;
    let beta = app.create_project(&cookie, "BET").await;
    let label = app.create_label(&cookie, "bug", "#ff0000").await;
    let label_id = label["id"].as_str().expect("label id");

    let first = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "First", "labels": [label_id]}),
        )
        .await;
    let second = app
        .create_task(&cookie, json!({"project_key": "ALP", "title": "Second"}))
        .await;
    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Todo", "status": "todo", "priority": "high", "due_date": "2026-12-01"}),
    )
    .await;
    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Working", "status": "in_progress"}),
    )
    .await;
    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Review", "status": "in_review"}),
    )
    .await;
    app.create_task(&cookie, json!({"project_key": "ALP", "title": "Blocked"}))
        .await;
    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Done", "status": "done"}),
    )
    .await;
    app.create_task(&cookie, json!({"project_key": "BET", "title": "Other"}))
        .await;

    app.cookie_post(
        "/api/tasks/ALP-2/move",
        Some(json!({"status": "backlog", "before": "ALP-1"})),
        &cookie,
    )
    .await;
    app.cookie_post(
        "/api/tasks/ALP-6/move",
        Some(json!({"status": "blocker"})),
        &cookie,
    )
    .await;
    app.cookie_post(
        "/api/tasks/ALP-1/checklist",
        Some(json!({"text": "first step"})),
        &cookie,
    )
    .await;
    let checked = app
        .cookie_post(
            "/api/tasks/ALP-7/checklist",
            Some(json!({"text": "finished step"})),
            &cookie,
        )
        .await;
    let checked_id = checked.body["id"].as_str().expect("checklist item id");
    app.cookie_patch(
        &format!("/api/tasks/ALP-7/checklist/{checked_id}"),
        json!({"done": true}),
        &cookie,
    )
    .await;
    app.cookie_post(
        "/api/tasks/ALP-2/dependencies",
        Some(json!({"blocked_by": ["ALP-1"]})),
        &cookie,
    )
    .await;
    let unblocked = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "Already unblocked"}),
        )
        .await;
    let dependency = app
        .cookie_post(
            &format!(
                "/api/tasks/{}/dependencies",
                unblocked["reference"].as_str().expect("reference")
            ),
            Some(json!({"blocked_by": ["ALP-7"]})),
            &cookie,
        )
        .await;
    assert_eq!(dependency.status, StatusCode::OK, "{:?}", dependency.body);
    assert_eq!(
        dependency.body["dependencies"]["blocked_by"][0]["reference"],
        "ALP-7"
    );

    let board = app
        .get("/api/projects/ALP/board", Some(&cookie), None)
        .await;
    assert_eq!(board.status, StatusCode::OK);
    assert_eq!(board.body["project"]["key"], "ALP");
    for column in [
        "backlog",
        "todo",
        "in_progress",
        "in_review",
        "blocker",
        "done",
    ] {
        assert!(board.body["columns"][column].is_array(), "{column}");
    }
    let backlog = board.body["columns"]["backlog"]
        .as_array()
        .expect("backlog");
    assert_eq!(backlog[0]["reference"], "ALP-2");
    assert_eq!(backlog[1]["reference"], "ALP-1");
    assert_eq!(backlog[1]["labels"][0]["name"], "bug");
    assert!(backlog[1].get("description").is_none());
    assert_eq!(backlog[1]["priority"], "normal");
    assert!(backlog[1]["attention"].is_boolean());
    assert!(backlog[1].get("claimed_by").is_some());
    assert!(backlog[1].get("session_status").is_some());
    assert!(backlog[1].get("blocked_from").is_some());
    assert_eq!(board.body["columns"]["todo"][0]["priority"], "high");
    assert_eq!(board.body["columns"]["todo"][0]["due_date"], "2026-12-01");
    assert_eq!(
        board.body["columns"]["blocker"][0]["blocked_from"],
        "backlog"
    );
    assert_eq!(backlog[1]["checklist_done"], 0);
    assert_eq!(backlog[1]["checklist_total"], 1);
    assert_eq!(backlog[0]["blocked"], true);
    assert_eq!(backlog[1]["blocked"], false);
    // A dependency on a done card is not a lock; a card without a checklist
    // reports zero progress instead of omitting the counters.
    assert_eq!(backlog[2]["reference"], unblocked["reference"]);
    assert_eq!(backlog[2]["blocked"], false);
    assert_eq!(backlog[2]["checklist_done"], 0);
    assert_eq!(backlog[2]["checklist_total"], 0);
    let done_card = &board.body["columns"]["done"][0];
    assert_eq!(done_card["reference"], "ALP-7");
    assert_eq!(done_card["checklist_done"], 1);
    assert_eq!(done_card["checklist_total"], 1);

    let other = app
        .get("/api/projects/BET/board", Some(&cookie), None)
        .await;
    assert_eq!(other.body["project"]["key"], "BET");
    assert_eq!(
        other.body["columns"]["backlog"]
            .as_array()
            .expect("backlog")
            .len(),
        1
    );

    let global = app.get("/api/board", Some(&cookie), None).await;
    assert_eq!(global.status, StatusCode::OK);
    let projects = global.body["projects"].as_array().expect("projects");
    assert_eq!(projects.len(), 2);
    assert_eq!(projects[0]["project"]["key"], "ALP");
    assert_eq!(projects[1]["project"]["key"], "BET");

    let filtered = app.get("/api/board?project=ALP", Some(&cookie), None).await;
    assert_eq!(
        filtered.body["projects"]
            .as_array()
            .expect("projects")
            .len(),
        1
    );
    assert_eq!(filtered.body["projects"][0]["project"]["id"], alpha["id"]);

    app.cookie_post(
        &format!("/api/projects/{}/archive", beta["id"].as_str().expect("id")),
        None,
        &cookie,
    )
    .await;
    let global = app.get("/api/board", Some(&cookie), None).await;
    assert_eq!(
        global.body["projects"].as_array().expect("projects").len(),
        1
    );
    let archived_board = app
        .get("/api/projects/BET/board", Some(&cookie), None)
        .await;
    assert_eq!(archived_board.status, StatusCode::OK);
    let _ = (first, second);
}

#[tokio::test]
async fn move_rejects_non_adjacent_neighbours() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    for title in ["First", "Second", "Third", "Fourth"] {
        app.create_task(&cookie, json!({"project_key": "ALP", "title": title}))
            .await;
    }

    let non_adjacent = app
        .cookie_post(
            "/api/tasks/ALP-4/move",
            Some(json!({"status": "backlog", "after": "ALP-1", "before": "ALP-3"})),
            &cookie,
        )
        .await;
    assert_eq!(non_adjacent.status, StatusCode::BAD_REQUEST);
    assert_eq!(non_adjacent.code(), "validation");

    let adjacent = app
        .cookie_post(
            "/api/tasks/ALP-4/move",
            Some(json!({"status": "backlog", "after": "ALP-1", "before": "ALP-2"})),
            &cookie,
        )
        .await;
    assert_eq!(adjacent.status, StatusCode::OK, "{:?}", adjacent.body);
    let list = app
        .get("/api/tasks?status=backlog", Some(&cookie), None)
        .await;
    assert_eq!(task_refs(&list.body), ["ALP-1", "ALP-4", "ALP-2", "ALP-3"]);
}

#[tokio::test]
async fn move_neighbour_error_uses_the_caller_reference() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Backlog card"}),
    )
    .await;
    let other = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "Todo card", "status": "todo"}),
        )
        .await;
    let other_id = other["id"].as_str().expect("id");

    for body in [
        json!({"status": "backlog", "before": "ALP-2"}),
        json!({"status": "backlog", "after": "ALP-2"}),
    ] {
        let response = app
            .cookie_post("/api/tasks/ALP-1/move", Some(body), &cookie)
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert_eq!(response.code(), "validation");
        let message = response.body["error"]["message"].as_str().expect("message");
        assert!(message.contains("ALP-2"), "{message}");
        assert!(!message.contains(other_id), "{message}");
    }

    let unknown = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"status": "backlog", "before": "ALP-999"})),
            &cookie,
        )
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    let message = unknown.body["error"]["message"].as_str().expect("message");
    assert!(message.contains("ALP-999"), "{message}");
}

#[tokio::test]
async fn session_link_is_human_only_and_records_the_activity() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    let task = app
        .create_task(&cookie, json!({"project_key": "ALP", "title": "Linked"}))
        .await;
    let reference = task["reference"].as_str().expect("reference");

    let linked = app
        .cookie_post(
            &format!("/api/tasks/{reference}/session"),
            Some(json!({
                "session_id": "ses_123",
                "directory": "/home/mathieu/worktrees/alp-1"
            })),
            &cookie,
        )
        .await;
    assert_eq!(linked.status, StatusCode::OK, "{:?}", linked.body);
    assert_eq!(linked.body["session_id"], "ses_123");
    assert_eq!(
        linked.body["session_directory"],
        "/home/mathieu/worktrees/alp-1"
    );
    assert_eq!(linked.body["session_status"], "running");
    assert!(activity_kinds(&linked.body)
        .iter()
        .any(|kind| kind == "session_linked"));
    let linked_entry = linked.body["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .find(|entry| entry["kind"] == "session_linked")
        .expect("session_linked activity");
    assert_eq!(linked_entry["data"]["session_id"], "ses_123");

    let idle = app
        .cookie_post(
            &format!("/api/tasks/{reference}/session"),
            Some(json!({
                "session_id": "ses_123",
                "directory": "/tmp",
                "status": "idle"
            })),
            &cookie,
        )
        .await;
    assert_eq!(idle.status, StatusCode::OK);
    assert_eq!(idle.body["session_status"], "idle");

    let invalid = app
        .cookie_post(
            &format!("/api/tasks/{reference}/session"),
            Some(json!({"session_id": "ses_1", "directory": "/tmp", "status": "busy"})),
            &cookie,
        )
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.code(), "validation");

    let missing = app
        .cookie_post(
            "/api/tasks/ALP-999/session",
            Some(json!({"session_id": "ses_1", "directory": "/tmp"})),
            &cookie,
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    let agent = app.create_token(&cookie, "agent", "agent").await;
    let agent = agent["token"].as_str().expect("token");
    let denied = app
        .request(
            Method::POST,
            &format!("/api/tasks/{reference}/session"),
            Some(json!({"session_id": "ses_2", "directory": "/tmp"})),
            None,
            Some(agent),
            None,
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn move_stores_and_clears_blocker_reasons() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    app.create_task(
        &cookie,
        json!({"project_key": "ALP", "title": "Blocked", "status": "todo"}),
    )
    .await;

    let blocked = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"status": "blocker", "reason": "  waiting on API key  "})),
            &cookie,
        )
        .await;
    assert_eq!(blocked.status, StatusCode::OK, "{:?}", blocked.body);
    assert_eq!(blocked.body["blocked_from"], "todo");
    assert_eq!(blocked.body["blocked_reason"], "waiting on API key");

    let restored = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"restore": true})),
            &cookie,
        )
        .await;
    assert_eq!(restored.status, StatusCode::OK);
    assert_eq!(restored.body["status"], "todo");
    assert!(restored.body["blocked_reason"].is_null());
    assert!(restored.body["blocked_from"].is_null());

    let outside = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"status": "in_review", "reason": "not blocked"})),
            &cookie,
        )
        .await;
    assert_eq!(outside.status, StatusCode::BAD_REQUEST);
    assert_eq!(outside.code(), "validation");

    let too_long = "x".repeat(501);
    let rejected = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"status": "blocker", "reason": too_long})),
            &cookie,
        )
        .await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);

    let cleared = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"status": "blocker"})),
            &cookie,
        )
        .await;
    assert_eq!(cleared.status, StatusCode::OK);
    assert!(cleared.body["blocked_reason"].is_null());

    let update = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"status": "blocker", "reason": "new reason"})),
            &cookie,
        )
        .await;
    assert_eq!(update.status, StatusCode::OK);
    assert_eq!(update.body["blocked_reason"], "new reason");
    assert_eq!(update.body["blocked_from"], "todo");

    // No status and a reason updates in place without clearing blocker metadata.
    let in_place = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"reason": "changed reason"})),
            &cookie,
        )
        .await;
    assert_eq!(in_place.status, StatusCode::OK, "{:?}", in_place.body);
    assert_eq!(in_place.body["blocked_reason"], "changed reason");
    assert_eq!(in_place.body["blocked_from"], "todo");

    // An explicit empty reason clears the stored text.
    let cleared_reason = app
        .cookie_post(
            "/api/tasks/ALP-1/move",
            Some(json!({"status": "blocker", "reason": "   "})),
            &cookie,
        )
        .await;
    assert_eq!(cleared_reason.status, StatusCode::OK);
    assert!(cleared_reason.body["blocked_reason"].is_null());
    assert_eq!(cleared_reason.body["blocked_from"], "todo");
}

#[tokio::test]
async fn agent_header_overrides_attribution_for_agent_tokens_only() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    app.create_task(&cookie, json!({"project_key": "ALP", "title": "Card"}))
        .await;
    let agent = app.create_token(&cookie, "build", "agent").await;
    let agent = agent["token"].as_str().expect("token");
    let human = app.create_token(&cookie, "human-cli", "human").await;
    let human = human["token"].as_str().expect("token");

    let commented = app
        .request_with_headers(
            Method::POST,
            "/api/tasks/ALP-1/comment",
            Some(json!({"body": "from reviewer"})),
            None,
            Some(agent),
            None,
            &[("X-Kanso-Agent", "reviewer")],
        )
        .await;
    assert_eq!(
        commented.status,
        StatusCode::CREATED,
        "{:?}",
        commented.body
    );
    assert_eq!(commented.body["author"], "reviewer");
    let detail = app.get("/api/tasks/ALP-1", None, Some(agent)).await;
    let comment_entry = detail.body["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .find(|entry| entry["kind"] == "commented")
        .expect("commented activity");
    assert_eq!(comment_entry["actor"], "reviewer");

    let proposal = app
        .request_with_headers(
            Method::POST,
            "/api/tasks",
            Some(json!({"project_key": "ALP", "title": "Idea"})),
            None,
            Some(agent),
            None,
            &[("X-Kanso-Agent", "planner")],
        )
        .await;
    assert_eq!(proposal.status, StatusCode::CREATED, "{:?}", proposal.body);
    assert_eq!(proposal.body["created_by"], "planner");
    assert_eq!(proposal.body["state"], "proposed");

    // A human token ignores the header.
    let human_comment = app
        .request_with_headers(
            Method::POST,
            "/api/tasks/ALP-1/comment",
            Some(json!({"body": "human"})),
            None,
            Some(human),
            None,
            &[("X-Kanso-Agent", "reviewer")],
        )
        .await;
    assert_eq!(human_comment.status, StatusCode::CREATED);
    assert_eq!(human_comment.body["author"], "human-cli");

    for value in ["", "   ", "ui", &"x".repeat(65)] {
        let response = app
            .request_with_headers(
                Method::GET,
                "/api/me",
                None,
                None,
                Some(agent),
                None,
                &[("X-Kanso-Agent", value)],
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{value:?}");
        assert_eq!(response.code(), "validation");
    }
}

#[tokio::test]
async fn export_dump_is_complete_versioned_and_human_only() {
    use std::collections::HashSet;

    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    let beta = app.create_project(&cookie, "BET").await;
    app.create_label(&cookie, "urgent", "#dc2626").await;

    let active = app
        .create_task(
            &cookie,
            json!({
                "project_key": "ALP",
                "title": "Active card",
                "description": "The active description",
                "priority": "high",
                "labels": ["urgent"]
            }),
        )
        .await;
    let active_ref = active["reference"].as_str().expect("reference").to_owned();
    app.cookie_post(
        &format!("/api/tasks/{active_ref}/checklist"),
        Some(json!({"text": "first step"})),
        &cookie,
    )
    .await;
    let linked = app
        .cookie_post(
            &format!("/api/tasks/{active_ref}/session"),
            Some(json!({
                "session_id": "ses_export",
                "directory": "/tmp/export",
                "status": "idle"
            })),
            &cookie,
        )
        .await;
    assert_eq!(linked.status, StatusCode::OK, "{:?}", linked.body);
    let comment = app
        .cookie_post(
            &format!("/api/tasks/{active_ref}/comment"),
            Some(json!({"body": "export comment"})),
            &cookie,
        )
        .await;
    assert_eq!(comment.status, StatusCode::CREATED, "{:?}", comment.body);

    let agent = app.create_token(&cookie, "export-agent", "agent").await;
    let agent_token = agent["token"].as_str().expect("token");
    let claimed = app
        .request(
            Method::POST,
            &format!("/api/tasks/{active_ref}/claim"),
            None,
            None,
            Some(agent_token),
            None,
        )
        .await;
    assert_eq!(claimed.status, StatusCode::OK, "{:?}", claimed.body);

    let dependent = app
        .create_task(
            &cookie,
            json!({"project_key": "ALP", "title": "Dependent card"}),
        )
        .await;
    let dependent_ref = dependent["reference"]
        .as_str()
        .expect("reference")
        .to_owned();
    let dependency = app
        .cookie_post(
            &format!("/api/tasks/{dependent_ref}/dependencies"),
            Some(json!({"blocked_by": [active_ref]})),
            &cookie,
        )
        .await;
    assert_eq!(dependency.status, StatusCode::OK, "{:?}", dependency.body);

    let archived = app
        .create_task(
            &cookie,
            json!({"project_key": "BET", "title": "Archived card"}),
        )
        .await;
    let archived_ref = archived["reference"]
        .as_str()
        .expect("reference")
        .to_owned();
    let archived = app
        .cookie_post(&format!("/api/tasks/{archived_ref}/archive"), None, &cookie)
        .await;
    assert_eq!(archived.status, StatusCode::OK);

    let proposal = app
        .request(
            Method::POST,
            "/api/tasks",
            Some(json!({"project_key": "ALP", "title": "Proposed card"})),
            None,
            Some(agent_token),
            None,
        )
        .await;
    assert_eq!(proposal.status, StatusCode::CREATED, "{:?}", proposal.body);
    let proposed_ref = proposal.body["reference"]
        .as_str()
        .expect("reference")
        .to_owned();

    let beta_id = beta["id"].as_str().expect("project id");
    let archived_project = app
        .cookie_post(&format!("/api/projects/{beta_id}/archive"), None, &cookie)
        .await;
    assert_eq!(archived_project.status, StatusCode::OK);

    // Cookie sessions need no CSRF origin header for this GET.
    let export = app.get("/api/export.json", Some(&cookie), None).await;
    assert_eq!(export.status, StatusCode::OK, "{:?}", export.body);
    let content_type = export
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert_eq!(content_type, "application/json");
    assert_eq!(
        export
            .headers
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    let disposition = export
        .headers
        .get(header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .expect("content disposition");
    let stamp = disposition
        .strip_prefix("attachment; filename=\"kanso-export-")
        .and_then(|value| value.strip_suffix(".json\""))
        .expect("export filename");
    assert_eq!(stamp.len(), 15, "{stamp}");
    assert_eq!(&stamp[8..9], "-", "{stamp}");
    assert!(stamp
        .chars()
        .enumerate()
        .all(|(index, character)| index == 8 || character.is_ascii_digit()));

    let body = &export.body;
    assert_eq!(body["version"], 1);
    assert!(body["exported_at"].as_i64().is_some());
    assert!(body.get("activity").is_none());

    let projects = body["projects"].as_array().expect("projects");
    let keys: Vec<&str> = projects
        .iter()
        .map(|project| project["key"].as_str().expect("key"))
        .collect();
    assert!(keys.contains(&"ALP"));
    assert!(keys.contains(&"BET"));
    let beta_project = projects
        .iter()
        .find(|project| project["key"] == "BET")
        .expect("archived project");
    assert!(beta_project["archived_at"].is_number());

    let labels = body["labels"].as_array().expect("labels");
    assert!(labels.iter().any(|label| label["name"] == "urgent"));

    let tasks = body["tasks"].as_array().expect("tasks");
    let references: HashSet<&str> = tasks
        .iter()
        .map(|task| task["reference"].as_str().expect("reference"))
        .collect();
    let by_ref = |reference: &str| {
        tasks
            .iter()
            .find(|task| task["reference"] == reference)
            .unwrap_or_else(|| panic!("task {reference} missing from export"))
    };

    let active_task = by_ref(&active_ref);
    assert_eq!(active_task["state"], "active");
    assert_eq!(active_task["priority"], "high");
    assert_eq!(active_task["description"], "The active description");
    assert_eq!(active_task["labels"][0]["name"], "urgent");
    assert_eq!(active_task["checklist"][0]["text"], "first step");
    assert_eq!(active_task["session_id"], "ses_export");
    assert_eq!(active_task["session_directory"], "/tmp/export");
    assert_eq!(active_task["session_status"], "idle");
    assert_eq!(active_task["claimed_by"], "export-agent");
    assert!(active_task.get("activity").is_none());

    let dependent_task = by_ref(&dependent_ref);
    assert_eq!(
        dependent_task["dependencies"]["blocked_by"][0]["reference"],
        active_ref
    );

    assert_eq!(by_ref(&archived_ref)["state"], "archived");
    assert!(by_ref(&archived_ref)["archived_at"].is_number());
    assert_eq!(by_ref(&proposed_ref)["state"], "proposed");

    let comments = body["comments"].as_array().expect("comments");
    let exported_comment = comments
        .iter()
        .find(|comment| comment["body"] == "export comment")
        .expect("exported comment");
    assert_eq!(exported_comment["task_reference"], active_ref);
    for comment in comments {
        let reference = comment["task_reference"]
            .as_str()
            .expect("comment task reference");
        assert!(references.contains(reference), "{reference}");
        assert!(tasks.iter().any(|task| task["id"] == comment["task_id"]));
    }
    for task in tasks {
        for dependency in task["dependencies"]["blocked_by"]
            .as_array()
            .expect("blocked_by")
        {
            assert!(references.contains(
                dependency["reference"]
                    .as_str()
                    .expect("dependency reference")
            ));
        }
    }

    let serialized = serde_json::to_string(body).expect("serialize export");
    let round_trip: Value = serde_json::from_str(&serialized).expect("parse export");
    assert_eq!(&round_trip, body);

    let denied = app.get("/api/export.json", None, Some(agent_token)).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.code(), "forbidden");

    let human = app.create_token(&cookie, "export-human", "human").await;
    let human_token = human["token"].as_str().expect("token");
    let allowed = app.get("/api/export.json", None, Some(human_token)).await;
    assert_eq!(allowed.status, StatusCode::OK, "{:?}", allowed.body);
    // The same dump is returned in the same stable order.
    for field in ["projects", "labels", "tasks", "comments"] {
        assert_eq!(allowed.body[field], body[field], "{field}");
    }
}

#[tokio::test]
async fn backup_writes_a_timestamped_copy_under_the_data_directory() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;

    let backup = app
        .request(
            Method::POST,
            "/api/backup",
            None,
            Some(&cookie),
            None,
            Some(ORIGIN),
        )
        .await;
    assert_eq!(backup.status, StatusCode::OK, "{:?}", backup.body);
    let path = backup.body["path"].as_str().expect("path");
    assert!(path.contains("/backups/kanso-"), "{path}");
    assert!(path.ends_with(".db"), "{path}");
    let size = backup.body["size"].as_u64().expect("size");
    assert!(size > 0);
    assert!(backup.body["created_at"].as_i64().is_some());
    assert!(Path::new(path).is_file(), "{path}");
    assert!(kanso::db::quick_check(Path::new(path)), "{path}");

    let nested = app
        .cookie_post(
            "/api/backup",
            Some(json!({"dir": "nested/backups"})),
            &cookie,
        )
        .await;
    assert_eq!(nested.status, StatusCode::OK, "{:?}", nested.body);
    let nested_path = nested.body["path"].as_str().expect("path");
    assert!(nested_path.contains("/nested/backups/"), "{nested_path}");
    assert!(Path::new(nested_path).is_file());

    let escape = app
        .cookie_post("/api/backup", Some(json!({"dir": "../escape"})), &cookie)
        .await;
    assert_eq!(escape.status, StatusCode::BAD_REQUEST);
    assert_eq!(escape.code(), "validation");

    let absolute_escape = app
        .cookie_post("/api/backup", Some(json!({"dir": "/etc"})), &cookie)
        .await;
    assert_eq!(absolute_escape.status, StatusCode::BAD_REQUEST);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = TempDir::new();
        symlink(outside.path(), app._dir.path().join("link")).expect("symlink");
        let symlink_escape = app
            .cookie_post("/api/backup", Some(json!({"dir": "link/backups"})), &cookie)
            .await;
        assert_eq!(symlink_escape.status, StatusCode::BAD_REQUEST);
        assert!(!outside.path().join("backups").exists());
    }

    let agent = app.create_token(&cookie, "agent", "agent").await;
    let agent = agent["token"].as_str().expect("token");
    let denied = app
        .request(Method::POST, "/api/backup", None, None, Some(agent), None)
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn agent_header_changes_attribution_but_not_claim_ownership() {
    let app = TestApp::new();
    let cookie = app.login().await;
    app.create_project(&cookie, "ALP").await;
    app.create_task(&cookie, json!({"project_key": "ALP", "title": "Owned"}))
        .await;
    let first = app.create_token(&cookie, "agent-one", "agent").await;
    let first = first["token"].as_str().expect("token");
    let second = app.create_token(&cookie, "agent-two", "agent").await;
    let second = second["token"].as_str().expect("token");

    let claimed = app
        .request_with_headers(
            Method::POST,
            "/api/tasks/ALP-1/claim",
            None,
            None,
            Some(first),
            None,
            &[("X-Kanso-Agent", "reviewer")],
        )
        .await;
    assert_eq!(claimed.status, StatusCode::OK, "{:?}", claimed.body);
    assert_eq!(claimed.body["claimed_by"], "agent-one");
    let claim_entry = claimed.body["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .find(|entry| entry["kind"] == "claimed")
        .expect("claimed activity");
    assert_eq!(claim_entry["actor"], "reviewer");

    // A different principal cannot release the claim by spoofing its name.
    let denied = app
        .request_with_headers(
            Method::POST,
            "/api/tasks/ALP-1/release",
            None,
            None,
            Some(second),
            None,
            &[("X-Kanso-Agent", "agent-one")],
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);

    let released = app
        .request_with_headers(
            Method::POST,
            "/api/tasks/ALP-1/release",
            None,
            None,
            Some(first),
            None,
            &[("X-Kanso-Agent", "reviewer")],
        )
        .await;
    assert_eq!(released.status, StatusCode::OK);
    assert!(released.body["claimed_by"].is_null());
    let release_entry = released.body["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .find(|entry| entry["kind"] == "released")
        .expect("released activity");
    assert_eq!(release_entry["actor"], "reviewer");
}
