//! Static shell integration tests: the SPA and every module it needs are
//! served with the right content type, and the version placeholder is replaced.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use axum::Router;
use kanso::auth::AuthConfig;
use kanso::db::Database;
use kanso::server::{router, AppState};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse";
static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Every asset the shell needs, with its expected content type exercised by the
/// serving test. New web modules must be added here.
const MODULES: [&str; 11] = [
    "/app.css",
    "/app.js",
    "/api.js",
    "/board.js",
    "/panel.js",
    "/imports.js",
    "/forms.js",
    "/dom.js",
    "/i18n/en.js",
    "/i18n/index.js",
    "/vendor/marked.min.js",
];

#[test]
fn secret_input_is_separate_from_drafts_and_refreshes_on_secret_events() {
    let panel = std::fs::read_to_string(repository_web_dir().join("panel.js")).unwrap();
    let section = panel
        .split("  renderSecrets(detail) {")
        .nth(1)
        .unwrap()
        .split("  renderLabels(detail) {")
        .next()
        .unwrap();
    assert!(section.contains("data-secret-input"));
    assert!(!section.contains("data-draft"));
    assert!(!section.contains("localStorage"));
    assert!(!section.contains("sessionStorage"));
    assert!(section.contains("field.value = \"\""));
    assert!(section.contains("api.retrySecret"));
    let app = std::fs::read_to_string(repository_web_dir().join("app.js")).unwrap();
    assert!(app.contains("\"secret.updated\""));
    assert!(app.contains("state.panel.reload({ force: true })"));
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        for _ in 0..100 {
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("kanso-ui-{}-{sequence}", std::process::id()));
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

fn repository_web_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web")
}

fn app(dir: &TempDir) -> Router {
    let database = Database::open(&dir.path().join("kanso.db")).expect("open database");
    let auth = AuthConfig::load(PASSWORD, None, dir.path()).expect("auth config");
    router(AppState::new(database, repository_web_dir(), auth))
}

async fn get(app: Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("call router");
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body")
        .to_vec();
    (status, content_type, body)
}

fn index_assets(html: &str) -> Vec<String> {
    let mut assets = Vec::new();
    for marker in ["src=\"", "href=\""] {
        let mut rest = html;
        while let Some(index) = rest.find(marker) {
            rest = &rest[index + marker.len()..];
            if let Some(end) = rest.find('"') {
                let url = &rest[..end];
                if url.starts_with('/') {
                    let path = url.split('?').next().unwrap_or(url);
                    // Only fetch concrete files, not future route links.
                    if Path::new(path).extension().is_some()
                        && !assets.iter().any(|asset| asset == path)
                    {
                        assets.push(path.to_owned());
                    }
                }
                rest = &rest[end + 1..];
            } else {
                break;
            }
        }
    }
    assets
}

fn web_scripts(root: &Path) -> Vec<String> {
    fn walk(root: &Path, directory: &Path, scripts: &mut Vec<String>) {
        for entry in std::fs::read_dir(directory).expect("read web directory") {
            let entry = entry.expect("read web entry");
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, scripts);
            } else if path.extension().and_then(|value| value.to_str()) == Some("js") {
                let relative = path
                    .strip_prefix(root)
                    .expect("web path")
                    .to_string_lossy()
                    .replace('\\', "/");
                scripts.push(format!("/{relative}"));
            }
        }
    }

    let mut scripts = Vec::new();
    walk(root, root, &mut scripts);
    scripts.sort();
    scripts
}

#[tokio::test]
async fn shell_and_every_module_are_served() {
    let dir = TempDir::new();
    let application = app(&dir);
    let version = env!("CARGO_PKG_VERSION");

    let (status, content_type, bytes) = get(application.clone(), "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "text/html; charset=utf-8");
    let html = String::from_utf8(bytes).expect("utf-8 shell");
    assert!(html.contains(version), "version should be injected");
    assert!(
        !html.contains("{{VERSION}}"),
        "no placeholder should remain"
    );
    assert!(html.contains("type=\"module\""));
    assert!(html.contains("/vendor/marked.min.js"));
    assert!(html.contains("/app.css"));
    assert!(html.contains("/app.js"));

    // Every URL the shell references must exist and must not fall back to the
    // SPA shell.
    for asset in index_assets(&html) {
        let (status, content_type, body) = get(application.clone(), &asset).await;
        assert_eq!(status, StatusCode::OK, "missing asset {asset}");
        assert!(!body.is_empty(), "empty asset {asset}");
        assert!(
            !content_type.starts_with("text/html"),
            "{asset} fell back to index.html"
        );
    }

    for module in MODULES {
        let (status, content_type, body) = get(application.clone(), module).await;
        assert_eq!(status, StatusCode::OK, "missing module {module}");
        assert!(!body.is_empty(), "empty module {module}");
        if module.ends_with(".css") {
            assert_eq!(content_type, "text/css; charset=utf-8", "{module}");
        } else {
            assert_eq!(content_type, "text/javascript; charset=utf-8", "{module}");
        }
    }
}

#[tokio::test]
async fn every_web_script_is_in_the_module_inventory() {
    let root = repository_web_dir();
    let on_disk = web_scripts(&root);
    let mut listed: Vec<String> = MODULES
        .iter()
        .filter(|path| path.ends_with(".js"))
        .map(|path| (*path).to_owned())
        .collect();
    listed.sort();
    assert_eq!(on_disk, listed, "add new web modules to the inventory test");
}

#[tokio::test]
async fn deep_links_serve_the_shell() {
    let dir = TempDir::new();
    for path in [
        "/",
        "/global",
        "/inbox",
        "/imports",
        "/settings/projects",
        "/settings/tokens",
        "/settings/integration",
        "/settings/export",
        "/p/ALP",
        "/p/ALP/task/42",
    ] {
        let (status, content_type, bytes) = get(app(&dir), path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(content_type, "text/html; charset=utf-8", "{path}");
        assert!(String::from_utf8(bytes)
            .expect("utf-8")
            .contains("id=\"app-root\""));
    }
}

#[tokio::test]
async fn a_missing_asset_does_not_fall_back_to_the_shell() {
    let dir = TempDir::new();
    let (status, content_type, _) = get(app(&dir), "/does-not-exist.js").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(content_type.starts_with("text/plain"));
}
