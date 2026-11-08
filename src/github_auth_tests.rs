use super::*;
use crate::test_support::TempDir;
use std::{os::unix::fs::PermissionsExt, sync::Arc};

fn fixture(dir: &TempDir, login: &str) -> std::path::PathBuf {
    let path = dir.path().join("fixture-gh");
    let script = format!(
        r##"#!/bin/sh
case "$1 $2" in
  '--version ') printf 'gh version 2.97.0 (fixture)\n' ;;
  'api --hostname') exit 1 ;;
  'auth status') printf '{{"hosts":{{}}}}\n' ;;
  'auth login') {login} ;;
  *) exit 1 ;;
esac
"##
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

async fn session(db: &Database, id: &str, expires: i64) {
    let id = id.to_owned();
    db.call(move |c| {
        c.execute(
            "INSERT INTO ui_sessions (id, created_at, expires_at, last_seen) VALUES (?1,0,?2,0)",
            rusqlite::params![id, expires],
        )?;
        Ok(())
    })
    .await
    .unwrap();
}

async fn wait_for(manager: &Manager, owner: &str, state: &str, code: bool) -> Progress {
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let value = manager.progress(owner).await;
            if value.state == state && (!code || value.code.is_some()) {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    match result {
        Ok(value) => value,
        Err(_) => panic!(
            "fixture login did not reach {state}; got {}",
            manager.progress(owner).await.state
        ),
    }
}

#[test]
fn parser_accepts_only_documented_fixture_shape() {
    assert_eq!(
        parse_code(b"! First copy your one-time code: TEST-1234\n"),
        Some("TEST-1234".into())
    );
    assert_eq!(parse_code(b"unrelated TEST-1234\n"), None);
    assert_eq!(
        parse_code(b"First copy your one-time code: test-1234\n"),
        None
    );
    assert_eq!(parse_code(b"First copy your one-time code: TEST-123"), None);
}

#[tokio::test]
async fn only_owner_sees_code_and_can_cancel_single_flight() {
    let dir = TempDir::new();
    let gh = fixture(
        &dir,
        "printf '! First copy your one-time code: TEST-1234\\n' >&2; exec sleep 60",
    );
    let manager = Arc::new(Manager::with_program(gh));
    let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
    session(&db, "owner", now_millis() + 60_000).await;
    session(&db, "other", now_millis() + 60_000).await;
    manager
        .start("owner".into(), false, db.clone())
        .await
        .unwrap();
    let own = wait_for(&manager, "owner", "pending", true).await;
    assert_eq!(own.code.as_deref(), Some("TEST-1234"));
    let other = manager.progress("other").await;
    assert_eq!(other.state, "pending");
    assert!(!other.owner);
    assert!(other.code.is_none());
    assert!(other.seconds_remaining.is_none());
    assert!(manager
        .start("other".into(), false, db.clone())
        .await
        .is_err());
    assert!(!manager.cancel("other").await);
    assert!(manager.cancel("owner").await);
    let cancelled = wait_for(&manager, "owner", "cancelled", false).await;
    assert!(cancelled.code.is_none());
    assert_eq!(manager.progress("other").await.state, "idle");
    manager.shutdown().await;
}

#[tokio::test]
async fn failed_or_unknown_output_never_exposes_process_diagnostics() {
    let dir = TempDir::new();
    let gh = fixture(&dir, "printf 'private fixture diagnostics\\n' >&2; exit 1");
    let manager = Arc::new(Manager::with_program(gh));
    let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
    session(&db, "owner", now_millis() + 60_000).await;
    manager.start("owner".into(), false, db).await.unwrap();
    let progress = wait_for(&manager, "owner", "failed", false).await;
    let json = serde_json::to_string(&progress).unwrap();
    assert!(!json.contains("private fixture diagnostics"));
    assert!(progress.code.is_none());
}

#[tokio::test]
async fn expired_session_terminates_attempt_and_clears_code() {
    let dir = TempDir::new();
    let gh = fixture(
        &dir,
        "printf '! First copy your one-time code: TEST-1234\\n' >&2; exec sleep 60",
    );
    let manager = Arc::new(Manager::with_program(gh));
    let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
    session(&db, "owner", now_millis() + 60_000).await;
    manager
        .start("owner".into(), false, db.clone())
        .await
        .unwrap();
    wait_for(&manager, "owner", "pending", true).await;
    db.call(|c| {
        c.execute("DELETE FROM ui_sessions WHERE id='owner'", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let result = wait_for(&manager, "owner", "cancelled", false).await;
    assert!(result.code.is_none());
}

#[tokio::test]
async fn shutdown_clears_the_active_attempt() {
    let dir = TempDir::new();
    let gh = fixture(
        &dir,
        "printf '! First copy your one-time code: TEST-1234\\n' >&2; exec sleep 60",
    );
    let manager = Arc::new(Manager::with_program(gh));
    let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
    session(&db, "owner", now_millis() + 60_000).await;
    manager.start("owner".into(), false, db).await.unwrap();
    wait_for(&manager, "owner", "pending", true).await;
    tokio::time::timeout(Duration::from_secs(5), manager.shutdown())
        .await
        .unwrap();
    let result = manager.progress("owner").await;
    assert_eq!(result.state, "cancelled");
    assert!(result.code.is_none());
}

#[tokio::test]
async fn environment_credentials_disable_browser_login() {
    let dir = TempDir::new();
    let marker = dir.path().join("spawned");
    let gh = dir.path().join("fixture-gh");
    let script = format!(
        r##"#!/bin/sh
case "$1 $2" in
 '--version ') printf 'gh version 2.97.0 (fixture)\n' ;;
 'api --hostname') printf '{{"login":"environment-account"}}\n' ;;
 'auth login') touch '{}'; exit 1 ;;
 *) exit 1 ;;
esac
"##,
        marker.display()
    );
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut manager = Manager::with_program(gh);
    manager.managed_override = Some(true);
    let manager = Arc::new(manager);
    let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
    session(&db, "owner", now_millis() + 60_000).await;
    manager.start("owner".into(), true, db).await.unwrap();
    wait_for(&manager, "owner", "failed", false).await;
    assert!(!marker.exists());
    let status = manager.status(false).await;
    assert!(status.operator_managed);
    assert_eq!(status.account.as_deref(), Some("environment-account"));
}

#[tokio::test]
async fn reconnect_requires_confirmation_before_login_process() {
    let dir = TempDir::new();
    let marker = dir.path().join("spawned");
    let gh = dir.path().join("fixture-gh");
    let script = format!(
        r##"#!/bin/sh
case "$1 $2" in
 '--version ') printf 'gh version 2.97.0 (fixture)\n' ;;
 'api --hostname') printf '{{"login":"fixture-user"}}\n' ;;
 'auth login') touch '{}'; exit 1 ;;
 *) exit 1 ;;
esac
"##,
        marker.display()
    );
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let manager = Arc::new(Manager::with_program(gh));
    let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
    session(&db, "owner", now_millis() + 60_000).await;
    let status = manager.status(false).await;
    assert_eq!(status.state, "connected");
    assert_eq!(status.account.as_deref(), Some("fixture-user"));
    manager.start("owner".into(), false, db).await.unwrap();
    wait_for(&manager, "owner", "failed", false).await;
    assert!(!marker.exists());
}

#[tokio::test]
async fn timeout_kills_and_reaps_child_and_clears_code() {
    let dir = TempDir::new();
    let pid_file = dir.path().join("fixture-pid");
    let gh = fixture(&dir, &format!("printf '%s' \"$$\" > '{}'; printf '! First copy your one-time code: TEST-1234\\n' >&2; exec sleep 60", pid_file.display()));
    let mut manager = Manager::with_program(gh);
    manager.attempt_limit = Duration::from_secs(1);
    let manager = Arc::new(manager);
    let db_path = dir.path().join("db.sqlite");
    let db = Database::open(&db_path).unwrap();
    session(&db, "owner", now_millis() + 60_000).await;
    manager.start("owner".into(), false, db).await.unwrap();
    wait_for(&manager, "owner", "pending", true).await;
    let pid: u32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
    let expired = wait_for(&manager, "owner", "expired", false).await;
    assert!(expired.code.is_none());
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    for suffix in ["", "-wal", "-shm"] {
        let path = format!("{}{suffix}", db_path.display());
        if let Ok(bytes) = std::fs::read(path) {
            assert!(!bytes.windows(9).any(|window| window == b"TEST-1234"));
        }
    }
}

#[tokio::test]
async fn invalid_stored_credential_can_only_reconnect_with_confirmation() {
    let dir = TempDir::new();
    let marker = dir.path().join("spawned");
    let gh = dir.path().join("fixture-gh");
    let script = format!(
        r##"#!/bin/sh
case "$1 $2" in
 '--version ') printf 'gh version 2.97.0 (fixture)\n' ;;
 'api --hostname') exit 1 ;;
 'auth status') printf '{{"hosts":{{"github.com":[{{"active":true,"state":"error","error":"private fixture diagnostic"}}]}}}}\n' ;;
 'auth login') touch '{}'; exit 1 ;;
 *) exit 1 ;;
esac
"##,
        marker.display()
    );
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let manager = Arc::new(Manager::with_program(gh));
    let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
    session(&db, "owner", now_millis() + 60_000).await;
    let status = manager.status(false).await;
    assert_eq!(status.state, "credential_error");
    assert!(!serde_json::to_string(&status)
        .unwrap()
        .contains("private fixture"));
    manager
        .start("owner".into(), false, db.clone())
        .await
        .unwrap();
    wait_for(&manager, "owner", "failed", false).await;
    assert!(!marker.exists());
    manager.start("owner".into(), true, db).await.unwrap();
    wait_for(&manager, "owner", "failed", false).await;
    assert!(marker.exists());
}

#[tokio::test]
async fn noisy_login_is_bounded_and_sanitized() {
    let dir = TempDir::new();
    let gh = fixture(
        &dir,
        "head -c 20000 /dev/zero | tr '\\0' X >&2; exec sleep 60",
    );
    let manager = Arc::new(Manager::with_program(gh));
    let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
    session(&db, "owner", now_millis() + 60_000).await;
    manager.start("owner".into(), false, db).await.unwrap();
    let result = wait_for(&manager, "owner", "failed", false).await;
    assert!(result.code.is_none());
    assert!(!serde_json::to_string(&result).unwrap().contains("XXX"));
}

#[tokio::test]
async fn incompatible_or_missing_cli_fails_without_login() {
    let dir = TempDir::new();
    let missing = Manager::with_program(dir.path().join("absent-gh"));
    assert_eq!(missing.status(false).await.state, "missing_cli");
    let gh = dir.path().join("fixture-gh");
    let marker = dir.path().join("spawned");
    std::fs::write(&gh, format!("#!/bin/sh\ncase \"$1 $2\" in\n '--version ') printf 'gh version 9.0.0 (fixture)\\n' ;;\n 'api --hostname') printf '{{\"login\":\"fixture-user\"}}\\n' ;;\n 'auth login') touch '{}';;\n *) exit 1;;\nesac\n", marker.display())).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let manager = Arc::new(Manager::with_program(gh));
    let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
    session(&db, "owner", now_millis() + 60_000).await;
    let status = manager.status(false).await;
    assert_eq!(status.state, "connected");
    assert!(!status.compatible);
    manager.start("owner".into(), true, db).await.unwrap();
    wait_for(&manager, "owner", "failed", false).await;
    assert!(!marker.exists());
}

#[tokio::test]
async fn approval_revalidates_actual_importer_identity() {
    let dir = TempDir::new();
    let approved = dir.path().join("approved");
    let gh = dir.path().join("fixture-gh");
    let script = format!(
        r##"#!/bin/sh
case "$1 $2" in
 '--version ') printf 'gh version 2.97.0 (fixture)\n' ;;
 'api --hostname') if [ -f '{0}' ]; then printf '{{"login":"actual-account"}}\n'; else exit 1; fi ;;
 'auth status') printf '{{"hosts":{{}}}}\n' ;;
 'auth login') printf '! First copy your one-time code: TEST-1234\n' >&2; sleep 0.3; touch '{0}' ;;
 *) exit 1 ;;
esac
"##,
        approved.display()
    );
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let manager = Arc::new(Manager::with_program(gh));
    let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
    session(&db, "owner", now_millis() + 60_000).await;
    assert_eq!(manager.status(false).await.state, "not_connected");
    manager.start("owner".into(), false, db).await.unwrap();
    wait_for(&manager, "owner", "pending", true).await;
    let done = wait_for(&manager, "owner", "completed", false).await;
    assert!(done.code.is_none());
    let actual = manager.status(true).await;
    assert_eq!(actual.state, "connected");
    assert_eq!(actual.account.as_deref(), Some("actual-account"));
}

#[tokio::test]
async fn cancel_after_approval_rechecks_active_account() {
    let dir = TempDir::new();
    let approved = dir.path().join("approved");
    let gh = dir.path().join("fixture-gh");
    let script = format!(
        r##"#!/bin/sh
case "$1 $2" in
 '--version ') printf 'gh version 2.97.0 (fixture)\n' ;;
 'api --hostname') if [ -f '{0}' ]; then printf '{{"login":"race-account"}}\n'; else exit 1; fi ;;
 'auth status') printf '{{"hosts":{{}}}}\n' ;;
 'auth login') touch '{0}'; printf '! First copy your one-time code: TEST-1234\n' >&2; exec sleep 60 ;;
 *) exit 1 ;;
esac
"##,
        approved.display()
    );
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let manager = Arc::new(Manager::with_program(gh));
    let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
    session(&db, "owner", now_millis() + 60_000).await;
    manager.start("owner".into(), false, db).await.unwrap();
    wait_for(&manager, "owner", "pending", true).await;
    assert!(manager.cancel("owner").await);
    wait_for(&manager, "owner", "cancelled", false).await;
    assert_eq!(
        manager.status(true).await.account.as_deref(),
        Some("race-account")
    );
}

#[tokio::test]
async fn explicit_refresh_bypasses_recent_automatic_status() {
    let dir = TempDir::new();
    let connected = dir.path().join("connected");
    let gh = dir.path().join("fixture-gh");
    let script = format!(
        r##"#!/bin/sh
case "$1 $2" in
 '--version ') printf 'gh version 2.97.0 (fixture)\n' ;;
 'api --hostname') if [ -f '{0}' ]; then printf '{{"login":"shell-login"}}\n'; else exit 1; fi ;;
 'auth status') printf '{{"hosts":{{}}}}\n' ;;
 *) exit 1 ;;
esac
"##,
        connected.display()
    );
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let manager = Manager::with_program(gh);
    assert_eq!(manager.status(false).await.state, "not_connected");
    std::fs::write(connected, b"fixture").unwrap();
    let status = manager.status(true).await;
    assert_eq!(status.account.as_deref(), Some("shell-login"));
}

#[tokio::test]
async fn http_ownership_progress_csrf_and_logout_cleanup() {
    use axum::{
        body::{to_bytes, Body},
        http::{header, Method, Request, StatusCode},
    };
    use tower::ServiceExt;
    let dir = TempDir::new();
    let gh = fixture(
        &dir,
        "printf '! First copy your one-time code: TEST-1234\\n' >&2; exec sleep 60",
    );
    let manager = Arc::new(Manager::with_program(gh));
    let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
    for id in ["owner", "other"] {
        session(&db, id, now_millis() + 60_000).await;
    }
    let auth =
        crate::auth::AuthConfig::load("fixture", Some("https://kanso.test"), dir.path()).unwrap();
    let owner = auth
        .session_cookie("owner")
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let other = auth
        .session_cookie("other")
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let mut state = crate::server::AppState::new(db, dir.path().into(), auth);
    state.github_auth = manager.clone();
    let app = crate::server::router(state);
    async fn request(
        app: &axum::Router,
        method: Method,
        path: &str,
        cookie: &str,
        origin: &str,
        body: &str,
    ) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::HOST, "kanso.test")
                    .header(header::ORIGIN, origin)
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
    let refreshed = request(
        &app,
        Method::POST,
        "/api/github/status",
        &owner,
        "https://kanso.test",
        "",
    )
    .await;
    assert_eq!(refreshed.status(), StatusCode::OK);
    assert_eq!(refreshed.headers()[header::CACHE_CONTROL], "no-store");
    let started = request(
        &app,
        Method::POST,
        "/api/github/login",
        &owner,
        "https://kanso.test",
        r#"{"confirmed":false}"#,
    )
    .await;
    assert_eq!(started.status(), StatusCode::OK);
    assert_eq!(started.headers()[header::CACHE_CONTROL], "no-store");
    wait_for(&manager, "owner", "pending", true).await;
    let theirs = request(
        &app,
        Method::GET,
        "/api/github/login",
        &other,
        "https://kanso.test",
        "",
    )
    .await;
    let theirs: serde_json::Value =
        serde_json::from_slice(&to_bytes(theirs.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(theirs["state"], "pending");
    assert_eq!(theirs["owner"], false);
    assert!(theirs.get("code").is_none());
    let theirs_cancel = request(
        &app,
        Method::DELETE,
        "/api/github/login",
        &other,
        "https://kanso.test",
        "",
    )
    .await;
    assert_eq!(theirs_cancel.status(), StatusCode::CONFLICT);
    let csrf = request(
        &app,
        Method::DELETE,
        "/api/github/login",
        &owner,
        "https://evil.test",
        "",
    )
    .await;
    assert_eq!(csrf.status(), StatusCode::FORBIDDEN);
    let mine = request(
        &app,
        Method::GET,
        "/api/github/login",
        &owner,
        "https://kanso.test",
        "",
    )
    .await;
    let mine: serde_json::Value =
        serde_json::from_slice(&to_bytes(mine.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(mine["code"], "TEST-1234");
    let logout = request(
        &app,
        Method::POST,
        "/api/auth/logout",
        &owner,
        "https://kanso.test",
        "",
    )
    .await;
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    let cancelled = wait_for(&manager, "owner", "cancelled", false).await;
    assert!(cancelled.code.is_none());
}

#[tokio::test]
async fn unknown_startup_output_fails_without_guessing() {
    let dir = TempDir::new();
    let gh = fixture(
        &dir,
        "printf 'unknown format TEST-1234\\n' >&2; exec sleep 60",
    );
    let mut manager = Manager::with_program(gh);
    manager.startup_limit = Duration::from_millis(150);
    let manager = Arc::new(manager);
    let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
    session(&db, "owner", now_millis() + 60_000).await;
    manager.start("owner".into(), false, db).await.unwrap();
    let result = wait_for(&manager, "owner", "failed", false).await;
    assert!(result.code.is_none());
}

#[test]
fn environment_detection_child() {
    if let Ok(expected) = std::env::var("KANSO_TEST_EXPECT_MANAGED") {
        assert_eq!(environment_managed(), expected == "yes");
    }
}

#[test]
fn real_environment_override_detection_is_isolated() {
    for (key, value, expected) in [
        ("GH_TOKEN", "fixture", "yes"),
        ("GITHUB_TOKEN", "fixture", "yes"),
        ("GH_TOKEN", "", "no"),
    ] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "github_auth::tests::environment_detection_child"])
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env(key, value)
            .env("KANSO_TEST_EXPECT_MANAGED", expected)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        assert!(child.status().unwrap().success());
    }
}
