//! Isolated GitLab configuration and shared-import workflow integration tests.
//! Remote filtering and HTTP semantics are covered by importer::gitlab fixtures.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use kanso::auth::AuthConfig;
use kanso::db::Database;
use kanso::importer::{Candidate, Provider, ProviderError, ProviderInfo};
use kanso::server::{router, AppState};
use serde_json::{json, Value};
use tower::ServiceExt;

const ORIGIN: &str = "https://kanso.example";
const PASSWORD: &str = "gitlab fixture password";
static SEQUENCE: AtomicU32 = AtomicU32::new(0);

struct TestApp {
    directory: PathBuf,
    app: Router,
    state: AppState,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

impl TestApp {
    fn new(fixtures: bool) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "kanso-gitlab-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let db = Database::open(&directory.join("kanso.db")).unwrap();
        let auth = AuthConfig::load(PASSWORD, Some(ORIGIN), &directory).unwrap();
        let mut state = AppState::new(db, directory.join("web"), auth);
        if fixtures {
            state = state.with_import_providers(vec![Arc::new(GitLabFixture)]);
        }
        let app = router(state.clone())
            .layer(MockConnectInfo(SocketAddr::from(([203, 0, 113, 17], 1234))));
        Self {
            directory,
            app,
            state,
        }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        cookie: Option<&str>,
        bearer: Option<&str>,
        origin: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "kanso.example");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(bearer) = bearer {
            request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn login(&self) -> String {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/auth/login")
            .header(header::HOST, "kanso.example")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"password":PASSWORD}).to_string()))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>, cookie: &str) -> Value {
        let (status, body) = self
            .request(method, path, body, Some(cookie), None, Some(ORIGIN))
            .await;
        assert!(status.is_success(), "{status}: {body}");
        body
    }

    async fn project(&self, cookie: &str, key: &str) -> String {
        self.call(
            Method::POST,
            "/api/projects",
            Some(json!({"key":key,"name":key})),
            cookie,
        )
        .await["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn configure(&self, cookie: &str, project: &str, repositories: Value) -> Value {
        self.call(
            Method::PUT,
            &format!("/api/projects/{project}/gitlab-repositories"),
            Some(repositories),
            cookie,
        )
        .await
    }

    async fn preview(&self, cookie: &str, project: &str, source: &str) -> Value {
        self.call(
            Method::POST,
            "/api/imports/preview",
            Some(preview(project, source)),
            cookie,
        )
        .await
    }

    async fn task_count(&self) -> i64 {
        self.state
            .db
            .call(|db| Ok(db.query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))?))
            .await
            .unwrap()
    }
}

struct GitLabFixture;
impl Provider for GitLabFixture {
    fn info(&self) -> ProviderInfo {
        kanso::importer::gitlab::GitLab.info()
    }
    fn project_info(
        &self,
        db: &rusqlite::Connection,
        project: &str,
    ) -> kanso::error::Result<ProviderInfo> {
        kanso::importer::gitlab::GitLab.project_info(db, project)
    }
    fn failure_message(&self) -> &'static str {
        kanso::importer::gitlab::GitLab.failure_message()
    }
    fn preview(
        &self,
        source: &str,
        filters: &HashMap<String, String>,
    ) -> Result<Vec<Candidate>, ProviderError> {
        if filters
            .get("user")
            .is_some_and(|user| user == "fixture-failure")
        {
            return Err(ProviderError);
        }
        let parts = source.split('|').collect::<Vec<_>>();
        let kind = if parts[2] == "issues" {
            "issue"
        } else {
            "merge_request"
        };
        Ok(vec![Candidate {
            identity: format!("{}|project:21|{kind}:42", parts[0]),
            kind: parts[2].to_owned(),
            title: "GitLab fixture".to_owned(),
            description: "**Untrusted** fixture body".to_owned(),
            source_url: format!("{}/{}/-/merge_requests/42", parts[0], parts[1]),
            invalid_reason: None,
        }])
    }
}

fn repository(instance: &str, path: &str) -> Value {
    json!({"instance":instance,"repository":path,"issues":true,"review":true,"follow":true})
}
fn source(instance: &str, path: &str, category: &str) -> String {
    format!("{instance}|{path}|{category}")
}
fn preview(project: &str, source: &str) -> Value {
    json!({"project_id":project,"provider":"gitlab","source":source,"filters":{}})
}
fn confirm(project: &str, source: &str, snapshot: &Value) -> Value {
    json!({"project_id":project,"provider":"gitlab","source":source,"preview_id":snapshot["preview_id"],"selected":[0]})
}

#[tokio::test]
async fn gitlab_multi_instance_configuration_is_independent_of_github() {
    let app = TestApp::new(false);
    let cookie = app.login().await;
    let project = app.project(&cookie, "GLA").await;
    let path = format!("/api/projects/{project}/gitlab-repositories");
    let github_path = format!("/api/projects/{project}/github-repositories");
    let github = json!([{"repository":"owner/repo","issues":true,"review":false,"follow":true}]);
    app.call(Method::PUT, &github_path, Some(github.clone()), &cookie)
        .await;
    let repositories = json!([
        repository("https://gitlab.com", "group/subgroup/repo"),
        repository("https://gitlab.example:8443", "group/subgroup/repo"),
        repository("https://gitlab.com", "group/second")
    ]);
    let saved = app.configure(&cookie, &project, repositories).await;
    assert_eq!(saved.as_array().unwrap().len(), 3);
    assert_eq!(app.call(Method::GET, &path, None, &cookie).await, saved);
    let providers = app
        .call(
            Method::GET,
            &format!("/api/imports/providers?project_id={project}"),
            None,
            &cookie,
        )
        .await;
    assert_eq!(providers.as_array().unwrap().len(), 2);
    assert!(providers
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "github" && p["sources"].as_array().unwrap().len() == 2));
    assert!(providers
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "gitlab" && p["sources"].as_array().unwrap().len() == 9));
    let mut changed = repository("https://gitlab.com", "group/renamed/repo");
    changed["review"] = json!(false);
    changed["issues"] = json!(false);
    app.configure(&cookie, &project, json!([changed])).await;
    let providers = app
        .call(
            Method::GET,
            &format!("/api/imports/providers?project_id={project}"),
            None,
            &cookie,
        )
        .await;
    let gitlab = providers
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "gitlab")
        .unwrap();
    assert_eq!(gitlab["sources"].as_array().unwrap().len(), 1);
    assert_eq!(
        gitlab["sources"][0]["id"],
        "https://gitlab.com|group/renamed/repo|follow"
    );
    app.configure(&cookie, &project, json!([])).await;
    assert_eq!(app.call(Method::GET, &path, None, &cookie).await, json!([]));
    assert_eq!(
        app.call(Method::GET, &github_path, None, &cookie).await,
        github
    );
}

#[tokio::test]
async fn gitlab_configuration_rejects_unsafe_duplicate_and_oversized_input_atomically() {
    let app = TestApp::new(false);
    let cookie = app.login().await;
    let project = app.project(&cookie, "GLB").await;
    let path = format!("/api/projects/{project}/gitlab-repositories");
    let original = json!([repository("https://gitlab.com", "group/repo")]);
    assert_eq!(
        app.configure(
            &cookie,
            &project,
            json!([repository("https://GITLAB.COM:443/", "group/repo")]),
        )
        .await,
        original,
    );
    for instance in [
        "http://gitlab.com",
        "https://token-secret@gitlab.com",
        "https://gitlab.com/path",
        "https://gitlab.com?token=secret",
        "https://gitlab.com#fragment",
        "https://gitlab.com|other",
    ] {
        let (status, response) = app
            .request(
                Method::PUT,
                &path,
                Some(json!([repository(instance, "group/repo")])),
                Some(&cookie),
                None,
                Some(ORIGIN),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{instance}: {response}");
        assert!(!response.to_string().contains("token-secret"));
    }
    for invalid_path in [
        "group",
        "../repo",
        "group/../repo",
        "group//repo",
        "group/repo|issues",
        "group/repo?token=secret",
        "group/repo\n",
    ] {
        let (status, _) = app
            .request(
                Method::PUT,
                &path,
                Some(json!([repository("https://gitlab.com", invalid_path)])),
                Some(&cookie),
                None,
                Some(ORIGIN),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid_path}");
    }
    let duplicate = json!([
        repository("https://gitlab.com", "group/repo"),
        repository("https://GITLAB.COM:443/", "GROUP/REPO")
    ]);
    assert_eq!(
        app.request(
            Method::PUT,
            &path,
            Some(duplicate),
            Some(&cookie),
            None,
            Some(ORIGIN)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let too_many = (0..51)
        .map(|i| repository("https://gitlab.com", &format!("group/repo{i}")))
        .collect::<Vec<_>>();
    assert_eq!(
        app.request(
            Method::PUT,
            &path,
            Some(json!(too_many)),
            Some(&cookie),
            None,
            Some(ORIGIN)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.call(Method::GET, &path, None, &cookie).await, original);
    app.call(
        Method::POST,
        &format!("/api/projects/{project}/archive"),
        None,
        &cookie,
    )
    .await;
    assert_eq!(
        app.request(
            Method::PUT,
            &path,
            Some(json!([])),
            Some(&cookie),
            None,
            Some(ORIGIN)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn gitlab_configuration_and_import_require_humans_and_csrf() {
    let app = TestApp::new(true);
    let cookie = app.login().await;
    let project = app.project(&cookie, "GLC").await;
    let path = format!("/api/projects/{project}/gitlab-repositories");
    app.configure(
        &cookie,
        &project,
        json!([repository("https://gitlab.com", "group/repo")]),
    )
    .await;
    let agent = app
        .call(
            Method::POST,
            "/api/tokens",
            Some(json!({"name":"gitlab-agent","scope":"agent"})),
            &cookie,
        )
        .await;
    let human = app
        .call(
            Method::POST,
            "/api/tokens",
            Some(json!({"name":"gitlab-human","scope":"human"})),
            &cookie,
        )
        .await;
    let source = source("https://gitlab.com", "group/repo", "issues");
    let snapshot = app.preview(&cookie, &project, &source).await;
    for (method, path, body) in [
        (Method::GET, path.clone(), None),
        (Method::PUT, path.clone(), Some(json!([]))),
        (Method::GET, "/api/imports/providers".into(), None),
        (
            Method::POST,
            "/api/imports/preview".into(),
            Some(preview(&project, &source)),
        ),
        (
            Method::POST,
            "/api/imports/confirm".into(),
            Some(confirm(&project, &source, &snapshot)),
        ),
    ] {
        assert_eq!(
            app.request(method.clone(), &path, body.clone(), None, None, None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.request(
                method.clone(),
                &path,
                body.clone(),
                None,
                agent["token"].as_str(),
                None
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        if method != Method::GET {
            for origin in [None, Some("https://evil.example")] {
                assert_eq!(
                    app.request(
                        method.clone(),
                        &path,
                        body.clone(),
                        Some(&cookie),
                        None,
                        origin
                    )
                    .await
                    .0,
                    StatusCode::FORBIDDEN
                );
            }
        }
    }
    assert_eq!(
        app.request(
            Method::GET,
            &path,
            None,
            None,
            human["token"].as_str(),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    let other_cookie = app.login().await;
    assert_eq!(
        app.request(
            Method::POST,
            "/api/imports/confirm",
            Some(confirm(&project, &source, &snapshot)),
            Some(&other_cookie),
            None,
            Some(ORIGIN)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(app.task_count().await, 0);
}

#[tokio::test]
async fn gitlab_import_identity_preserves_archived_edits_and_provenance_across_renames_and_categories(
) {
    let app = TestApp::new(true);
    let cookie = app.login().await;
    let project = app.project(&cookie, "GLD").await;
    let other_project = app.project(&cookie, "GLE").await;
    app.configure(
        &cookie,
        &project,
        json!([repository("https://gitlab.com", "group/repo")]),
    )
    .await;
    let review = source("https://gitlab.com", "group/repo", "review");
    let snapshot = app.preview(&cookie, &project, &review).await;
    assert_eq!(app.task_count().await, 0);
    let request = confirm(&project, &review, &snapshot);
    let (first, second) = tokio::join!(
        app.call(
            Method::POST,
            "/api/imports/confirm",
            Some(request.clone()),
            &cookie
        ),
        app.call(Method::POST, "/api/imports/confirm", Some(request), &cookie)
    );
    let statuses = [
        first[0]["status"].as_str().unwrap(),
        second[0]["status"].as_str().unwrap(),
    ];
    assert!(statuses.contains(&"created") && statuses.contains(&"skipped"));
    let reference = first[0]["reference"].as_str().unwrap();
    let detail_path = format!("/api/tasks/{reference}");
    let detail = app.call(Method::GET, &detail_path, None, &cookie).await;
    assert_eq!(detail["status"], "backlog");
    assert_eq!(detail["priority"], "normal");
    assert_eq!(detail["source"], "human");
    assert_eq!(detail["provenance"]["provider"], "gitlab");
    assert_eq!(
        detail["provenance"]["source_url"],
        "https://gitlab.com/group/repo/-/merge_requests/42"
    );
    assert!(detail["activity"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["kind"] == "created"));
    app.call(
        Method::PATCH,
        &detail_path,
        Some(json!({"title":"Local title","description":"Local description"})),
        &cookie,
    )
    .await;
    app.call(
        Method::POST,
        &format!("{detail_path}/archive"),
        None,
        &cookie,
    )
    .await;
    app.configure(
        &cookie,
        &project,
        json!([
            repository("https://gitlab.com", "renamed/nested/repo"),
            repository("https://gitlab.example", "renamed/nested/repo")
        ]),
    )
    .await;
    let follow = source("https://gitlab.com", "renamed/nested/repo", "follow");
    let renamed = app.preview(&cookie, &project, &follow).await;
    assert_eq!(renamed["candidates"][0]["existing"], reference);
    let skipped = app
        .call(
            Method::POST,
            "/api/imports/confirm",
            Some(confirm(&project, &follow, &renamed)),
            &cookie,
        )
        .await;
    assert_eq!(skipped[0]["status"], "skipped");
    let preserved = app.call(Method::GET, &detail_path, None, &cookie).await;
    assert_eq!(preserved["title"], "Local title");
    assert_eq!(preserved["description"], "Local description");
    assert_eq!(preserved["state"], "archived");
    assert_eq!(preserved["provenance"], detail["provenance"]);
    let distinct = source("https://gitlab.example", "renamed/nested/repo", "follow");
    let another_instance = app.preview(&cookie, &project, &distinct).await;
    assert!(another_instance["candidates"][0]["existing"].is_null());
    assert_eq!(
        app.call(
            Method::POST,
            "/api/imports/confirm",
            Some(confirm(&project, &distinct, &another_instance)),
            &cookie
        )
        .await[0]["status"],
        "created"
    );
    app.configure(
        &cookie,
        &other_project,
        json!([repository("https://gitlab.com", "renamed/nested/repo")]),
    )
    .await;
    let another_project = app.preview(&cookie, &other_project, &follow).await;
    assert_eq!(
        app.call(
            Method::POST,
            "/api/imports/confirm",
            Some(confirm(&other_project, &follow, &another_project)),
            &cookie
        )
        .await[0]["status"],
        "created"
    );
    let exported = app
        .call(Method::GET, "/api/export.json", None, &cookie)
        .await;
    let tasks = exported["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 3);
    assert!(tasks
        .iter()
        .all(|task| task["provenance"]["provider"] == "gitlab"));
    assert_eq!(
        tasks
            .iter()
            .find(|task| task["reference"] == reference)
            .unwrap()["provenance"],
        detail["provenance"]
    );
}

#[tokio::test]
async fn gitlab_snapshot_selection_source_changes_and_provider_failures_create_nothing() {
    let app = TestApp::new(true);
    let cookie = app.login().await;
    let project = app.project(&cookie, "GLF").await;
    app.configure(
        &cookie,
        &project,
        json!([repository("https://gitlab.com", "group/repo")]),
    )
    .await;
    let issues = source("https://gitlab.com", "group/repo", "issues");
    let snapshot = app.preview(&cookie, &project, &issues).await;
    let mut forged = confirm(&project, &issues, &snapshot);
    forged["candidates"] = json!([{"title":"Forged"}]);
    assert_eq!(
        app.request(
            Method::POST,
            "/api/imports/confirm",
            Some(forged),
            Some(&cookie),
            None,
            Some(ORIGIN)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    for selection in [json!([]), json!([0, 0]), json!([1])] {
        let mut invalid = confirm(&project, &issues, &snapshot);
        invalid["selected"] = selection;
        assert_eq!(
            app.request(
                Method::POST,
                "/api/imports/confirm",
                Some(invalid),
                Some(&cookie),
                None,
                Some(ORIGIN)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    app.configure(&cookie, &project, json!([])).await;
    let disabled = app
        .call(
            Method::POST,
            "/api/imports/confirm",
            Some(confirm(&project, &issues, &snapshot)),
            &cookie,
        )
        .await;
    assert_eq!(disabled[0]["status"], "failed");
    app.configure(
        &cookie,
        &project,
        json!([repository("https://gitlab.com", "group/repo")]),
    )
    .await;
    let mut failure = preview(&project, &issues);
    failure["filters"] = json!({"user":"fixture-failure"});
    let (status, body) = app
        .request(
            Method::POST,
            "/api/imports/preview",
            Some(failure),
            Some(&cookie),
            None,
            Some(ORIGIN),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "provider_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("GitLab"));
    assert_eq!(
        app.request(
            Method::POST,
            "/api/imports/confirm",
            Some(confirm(&project, &issues, &snapshot)),
            Some(&cookie),
            None,
            Some(ORIGIN)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(app.task_count().await, 0);
}

#[tokio::test]
async fn gitlab_nested_paths_can_exceed_legacy_github_source_metadata_limits() {
    let app = TestApp::new(true);
    let cookie = app.login().await;
    let project = app.project(&cookie, "GLG").await;
    let path = format!("{}/repo", vec!["nested_group"; 20].join("/"));
    app.configure(
        &cookie,
        &project,
        json!([repository("https://gitlab.com", &path)]),
    )
    .await;
    let metadata = app
        .call(
            Method::GET,
            &format!("/api/imports/providers?project_id={project}"),
            None,
            &cookie,
        )
        .await;
    assert_eq!(metadata[0]["sources"].as_array().unwrap().len(), 3);
    let snapshot = app
        .preview(
            &cookie,
            &project,
            &source("https://gitlab.com", &path, "issues"),
        )
        .await;
    assert_eq!(snapshot["candidates"].as_array().unwrap().len(), 1);
}
