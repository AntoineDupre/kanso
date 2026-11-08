use super::{load, update, UpdateSettings};
use crate::adapter::openchamber::OpenChamber;
use crate::api::{ApiJson, Auth};
use crate::auth::{AuthConfig, Identity, IdentityKind, Scope};
use crate::db::Database;
use crate::server::AppState;
use crate::test_support::TempDir;
use axum::extract::State;

fn state(dir: &TempDir) -> AppState {
    let db = Database::open(&dir.path().join("kanso.db")).unwrap();
    let auth = AuthConfig::load("password", None, dir.path()).unwrap();
    AppState::new(db, dir.path().join("web"), auth)
}

fn human() -> Auth {
    Auth(Identity {
        kind: IdentityKind::Ui,
        name: "ui".into(),
        scope: Scope::Human,
        display_name: None,
        session_id: None,
        token_id: None,
    })
}

#[tokio::test]
async fn stored_connection_survives_restart_and_overrides_environment() {
    let dir = TempDir::new();
    let app = state(&dir).with_openchamber(OpenChamber::new("http://environment", "env_token"));
    load(&app).unwrap();
    assert_eq!(app.openchamber().unwrap().url(), "http://environment");
    update(
        State(app.clone()),
        human(),
        ApiJson(UpdateSettings {
            url: "http://stored".into(),
            token: Some("stored_token".into()),
        }),
    )
    .await
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.path().join("openchamber-connection.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let restarted =
        state(&dir).with_openchamber(OpenChamber::new("http://other-env", "other_token"));
    load(&restarted).unwrap();
    assert_eq!(restarted.openchamber().unwrap().url(), "http://stored");
    assert_eq!(restarted.openchamber().unwrap().token(), "stored_token");
}
