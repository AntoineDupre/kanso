use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use axum::{
    extract::{Path as UrlPath, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use tokio::fs;

use crate::server::AppState;

/// Resolve the directory holding the static UI.
///
/// The container keeps `web/` next to the binary; a development checkout runs
/// the binary from `target/debug`, so fall back to `./web`.
pub fn default_web_dir() -> PathBuf {
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            let candidate = directory.join("web");
            if candidate.is_dir() {
                return candidate;
            }
        }
    }
    PathBuf::from("web")
}

pub async fn index(State(state): State<AppState>) -> Response {
    serve_index(&state.web_dir).await
}

pub async fn asset(State(state): State<AppState>, UrlPath(raw): UrlPath<String>) -> Response {
    let Some(relative) = sanitize(&raw) else {
        return not_found();
    };

    let is_api = matches!(
        relative.components().next(),
        Some(Component::Normal(name)) if name.to_str() == Some("api")
    );
    if is_api {
        return api_not_found();
    }

    if relative == Path::new("index.html") {
        return serve_index(&state.web_dir).await;
    }

    let file = state.web_dir.join(&relative);
    match fs::read(&file).await {
        Ok(bytes) => asset_response(&file, bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if relative.extension().is_some() {
                not_found()
            } else {
                serve_index(&state.web_dir).await
            }
        }
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::IsADirectory | ErrorKind::NotADirectory
            ) =>
        {
            not_found()
        }
        Err(error) => {
            tracing::error!(path = %file.display(), %error, "failed to read web asset");
            (StatusCode::INTERNAL_SERVER_ERROR, "web assets unavailable").into_response()
        }
    }
}

/// Reject traversal components and build a relative path from the URL.
fn sanitize(raw: &str) -> Option<PathBuf> {
    let mut path = PathBuf::new();
    for segment in raw.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains('\\') || segment.contains('\0') {
            return None;
        }
        path.push(segment);
    }
    Some(path)
}

async fn serve_index(web_dir: &Path) -> Response {
    let index = web_dir.join("index.html");
    match fs::read(&index).await {
        Ok(bytes) => {
            let html =
                String::from_utf8_lossy(&bytes).replace("{{VERSION}}", env!("CARGO_PKG_VERSION"));
            (
                [
                    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-cache"),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                ],
                html,
            )
                .into_response()
        }
        Err(error) => {
            tracing::error!(path = %index.display(), %error, "failed to read web/index.html");
            (StatusCode::INTERNAL_SERVER_ERROR, "web assets unavailable").into_response()
        }
    }
}

fn asset_response(path: &Path, bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type(path)),
            // Assets are not content-hashed yet, so never cache them across a
            // deploy. Revisit when the UI introduces fingerprinted URLs.
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response()
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("json") | Some("map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("txt") | Some("md") => "text/plain; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not found").into_response()
}

fn api_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": { "code": "not_found", "message": "Not found" }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::router;
    use crate::test_support::TempDir;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    fn app(dir: &TempDir) -> axum::Router {
        let web_dir = dir.path().join("web");
        std::fs::create_dir_all(&web_dir).expect("create web directory");
        std::fs::write(web_dir.join("index.html"), "<h1>Kanso {{VERSION}}</h1>")
            .expect("write index");
        std::fs::write(web_dir.join("app.js"), "console.log('kanso');").expect("write app.js");
        std::fs::create_dir_all(web_dir.join("assets")).expect("create assets directory");
        let database =
            crate::db::Database::open(&dir.path().join("kanso.db")).expect("open database");
        let auth = crate::auth::AuthConfig::load("secret", None, dir.path()).expect("auth config");
        router(AppState::new(database, web_dir, auth))
    }

    async fn get(app: axum::Router, uri: &str) -> Response {
        app.oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("call router")
    }

    async fn body_text(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        String::from_utf8(bytes.to_vec()).expect("utf-8 body")
    }

    #[test]
    fn sanitize_rejects_traversal() {
        assert!(sanitize("../etc/passwd").is_none());
        assert!(sanitize("assets/../../secret").is_none());
        assert!(sanitize("assets\\secret").is_none());
        assert_eq!(
            sanitize("assets/app.js"),
            Some(PathBuf::from("assets/app.js"))
        );
        assert_eq!(
            sanitize("/assets//app.js"),
            Some(PathBuf::from("assets/app.js"))
        );
    }

    #[test]
    fn content_types_are_explicit() {
        assert_eq!(
            content_type(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("app.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("app.css")),
            "text/css; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("logo.svg")), "image/svg+xml");
        assert_eq!(
            content_type(Path::new("unknown.bin")),
            "application/octet-stream"
        );
    }

    #[tokio::test]
    async fn spa_routes_fall_back_to_index_with_the_version() {
        let dir = TempDir::new();
        let response = get(app(&dir), "/p/ALP/task/42").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(
            body_text(response).await,
            format!("<h1>Kanso {}</h1>", env!("CARGO_PKG_VERSION"))
        );
    }

    #[tokio::test]
    async fn assets_are_served_with_their_content_type() {
        let dir = TempDir::new();
        let response = get(app(&dir), "/app.js").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(body_text(response).await, "console.log('kanso');");
    }

    #[tokio::test]
    async fn api_paths_never_fall_back_to_index() {
        let dir = TempDir::new();
        let response = get(app(&dir), "/api/unknown").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(body_text(response).await.contains("not_found"));
    }

    #[tokio::test]
    async fn missing_asset_with_an_extension_is_not_replaced_by_index() {
        let dir = TempDir::new();
        let response = get(app(&dir), "/missing.css").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn direct_index_request_gets_the_version_substitution() {
        let dir = TempDir::new();
        let response = get(app(&dir), "/index.html").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_text(response).await,
            format!("<h1>Kanso {}</h1>", env!("CARGO_PKG_VERSION"))
        );
    }

    #[tokio::test]
    async fn dot_segments_do_not_bypass_the_api_guard() {
        let dir = TempDir::new();
        let response = get(app(&dir), "/./api/unknown").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(body_text(response).await.contains("not_found"));
    }

    #[tokio::test]
    async fn percent_encoded_traversal_cannot_escape_the_web_root() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("secret.txt"), "top secret").expect("write secret");

        let response = get(app(&dir), "/%2e%2e/secret.txt").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!body_text(response).await.contains("top secret"));
    }

    #[tokio::test]
    async fn directories_and_paths_under_files_are_not_served() {
        let dir = TempDir::new();

        let response = get(app(&dir), "/assets").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = get(app(&dir), "/app.js/extra").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
