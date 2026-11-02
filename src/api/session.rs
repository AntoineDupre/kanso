use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{now_millis, response_with_cookie, ApiError, ApiJson, Auth, PeerAddr};
use crate::auth::{self, SESSION_MAX_AGE_MILLIS};
use crate::ids;
use crate::server::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginRequest {
    password: String,
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    ApiJson(payload): ApiJson<LoginRequest>,
) -> Result<Response, ApiError> {
    let ip = auth::client_ip(&headers, peer);
    if !state.login_limiter.try_begin(&ip) {
        return Err(ApiError::rate_limited());
    }

    if !state.auth.password_matches(&payload.password) {
        return Err(ApiError::unauthorized("invalid password"));
    }
    state.login_limiter.reset(&ip);

    let session_id = ids::base64_url(&ids::random_bytes::<32>()?);
    let session_id_for_query = session_id.clone();
    let now = now_millis();
    state
        .db
        .call(move |connection| {
            connection.execute(
                "INSERT INTO ui_sessions (id, created_at, expires_at, last_seen)
                 VALUES (?1, ?2, ?3, ?2)",
                params![session_id_for_query, now, now + SESSION_MAX_AGE_MILLIS],
            )?;
            Ok(())
        })
        .await?;

    Ok(response_with_cookie(
        StatusCode::NO_CONTENT,
        &state.auth.session_cookie(&session_id),
    ))
}

pub async fn logout(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Response, ApiError> {
    if let Some(session_id) = identity.session_id {
        state
            .db
            .call(move |connection| {
                connection.execute("DELETE FROM ui_sessions WHERE id = ?1", [session_id])?;
                Ok(())
            })
            .await?;
    }
    Ok(response_with_cookie(
        StatusCode::NO_CONTENT,
        &state.auth.clear_cookie(),
    ))
}

#[derive(Serialize)]
pub struct Me {
    kind: &'static str,
    name: String,
    scope: &'static str,
}

pub async fn me(Auth(identity): Auth) -> Json<Me> {
    Json(Me {
        kind: identity.kind.as_str(),
        name: identity.name,
        scope: identity.scope.as_str(),
    })
}
