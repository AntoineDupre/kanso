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
            Method::POST,
            "/api/tasks".to_owned(),
            Some(json!({"project_key": "ALP", "title": "Nope"})),
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
