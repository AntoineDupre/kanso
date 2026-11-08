use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use super::{require_human, ApiError, ApiJson, Auth};
use crate::adapter::openchamber::{OpenChamber, OpenChamberError};
use crate::server::AppState;

#[cfg(test)]
#[path = "integration_tests.rs"]
mod tests;

#[derive(Serialize, Deserialize)]
struct StoredSettings {
    url: String,
    token: String,
}

fn settings_path(state: &AppState) -> std::path::PathBuf {
    state.db_path.with_file_name("openchamber-connection.json")
}

/// Load a stored connection, if one exists. Invalid settings fail startup rather
/// than silently falling back to an expired environment token.
pub(crate) fn load(state: &AppState) -> Result<(), String> {
    let path = settings_path(state);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "could not read OpenChamber settings at {}: {error}",
                path.display()
            ))
        }
    };
    let settings: StoredSettings = serde_json::from_slice(&bytes).map_err(|_| {
        format!(
            "invalid OpenChamber settings at {}; check the private settings file",
            path.display()
        )
    })?;
    validate(&settings.url, &settings.token).map_err(|_| {
        format!(
            "invalid OpenChamber settings at {}; check the private settings file",
            path.display()
        )
    })?;
    state.set_openchamber(Some(OpenChamber::new(&settings.url, &settings.token)));
    Ok(())
}

fn validate(url: &str, token: &str) -> Result<(), ApiError> {
    let uri: axum::http::Uri = url
        .parse()
        .map_err(|_| ApiError::validation("invalid OpenChamber URL"))?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.host().is_none()
        || uri.authority().is_some_and(|a| a.as_str().contains('@'))
        || uri.query().is_some()
        || url.contains('#')
    {
        return Err(ApiError::validation(
            "OpenChamber URL must be an HTTP(S) URL without credentials, query or fragment",
        ));
    }
    if token.is_empty()
        || token.len() > 4096
        || token.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ApiError::validation("invalid OpenChamber token"));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct UpdateSettings {
    url: String,
    token: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    Auth(identity): Auth,
    ApiJson(input): ApiJson<UpdateSettings>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_human(&identity)?;
    let guard = state.openchamber_settings_lock.clone().lock_owned().await;
    let existing = state.openchamber();
    if input.token.is_none()
        && existing
            .as_ref()
            .is_some_and(|a| a.url() != input.url.trim().trim_end_matches('/'))
    {
        return Err(ApiError::validation(
            "Changing the URL requires a replacement token",
        ));
    }
    // An unchanged URL and an omitted token are a no-op. In particular, do
    // not persist an environment-supplied token and accidentally shadow future
    // environment rotations.
    if input.token.is_none() && existing.is_some() {
        return Ok(axum::http::StatusCode::NO_CONTENT);
    }
    let token = input
        .token
        .as_deref()
        .map(str::trim)
        .or_else(|| existing.as_ref().map(|adapter| adapter.token()))
        .ok_or_else(|| ApiError::validation("a token is required"))?;
    let url = input.url.trim();
    validate(url, token)?;
    let settings = StoredSettings {
        url: url.to_owned(),
        token: token.to_owned(),
    };
    save(state, settings, guard).await
}

async fn save(
    state: AppState,
    settings: StoredSettings,
    guard: tokio::sync::OwnedMutexGuard<()>,
) -> Result<axum::http::StatusCode, ApiError> {
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let bytes = serde_json::to_vec(&settings)
            .map_err(|_| ApiError::internal("could not save settings"))?;
        crate::fileutil::write_atomic(&settings_path(&state), &bytes, 0o600).map_err(|_| {
            ApiError::internal(
                "could not save settings; check data directory permissions and free space",
            )
        })?;
        state.set_openchamber(Some(OpenChamber::new(&settings.url, &settings.token)));
        Ok(axum::http::StatusCode::NO_CONTENT)
    })
    .await
    .map_err(|_| ApiError::internal("could not save settings"))?
}

#[derive(Deserialize)]
pub struct PairingInput {
    link: String,
}

pub async fn regenerate(
    State(state): State<AppState>,
    Auth(identity): Auth,
    ApiJson(input): ApiJson<PairingInput>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_human(&identity)?;
    let guard = state.openchamber_settings_lock.clone().lock_owned().await;
    let adapter = state.openchamber().ok_or_else(|| {
        ApiError::service_unavailable(
            "integration_disabled",
            "Configure OpenChamber before regenerating a token",
        )
    })?;
    let url = adapter.url().to_owned();
    let token = crate::adapter::openchamber_pairing::redeem(url.clone(), input.link)
        .await
        .map_err(|message| match message {
            "Invalid pairing link" => ApiError::validation(message),
            "OpenChamber rejected pairing; create a new pairing link" => {
                ApiError::bad_gateway(message)
            }
            _ => ApiError::service_unavailable("pairing_failed", message),
        })?;
    validate(&url, &token)?;
    save(state, StoredSettings { url, token }, guard).await
}

#[derive(Serialize)]
pub struct TestResult {
    status: &'static str,
}

pub async fn test(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<TestResult>, ApiError> {
    require_human(&identity)?;
    let adapter = state.openchamber().ok_or_else(|| {
        ApiError::service_unavailable("integration_disabled", "OpenChamber is not configured")
    })?;
    let status = match adapter.test_connection().await {
        Ok(()) => "ok",
        Err(OpenChamberError::Http {
            status: 401 | 403, ..
        }) => "authentication_error",
        Err(OpenChamberError::Transport(_)) => "transport_error",
        Err(OpenChamberError::Protocol(_) | OpenChamberError::Http { .. }) => "protocol_error",
    };
    Ok(Json(TestResult { status }))
}

#[derive(Serialize)]
pub struct Integration {
    pub openchamber: OpenChamberInfo,
}

/// Disabled is exactly `{enabled:false}`; enabled always carries all four keys,
/// including `version:null` when the probe cannot read one.
#[derive(Serialize)]
#[serde(untagged)]
pub enum OpenChamberInfo {
    Disabled {
        enabled: bool,
    },
    Enabled {
        enabled: bool,
        url: String,
        reachable: bool,
        version: Option<String>,
    },
}

/// `GET /api/integration` — never fails when OpenChamber is down, and never
/// returns the configured token.
pub async fn status(
    State(state): State<AppState>,
    _auth: Auth,
) -> Result<Json<Integration>, ApiError> {
    let Some(openchamber) = state.openchamber() else {
        return Ok(Json(Integration {
            openchamber: OpenChamberInfo::Disabled { enabled: false },
        }));
    };

    let probe = openchamber.version().await;
    Ok(Json(Integration {
        openchamber: OpenChamberInfo::Enabled {
            enabled: true,
            url: openchamber.url().to_owned(),
            reachable: probe.reachable,
            version: probe.version,
        },
    }))
}
