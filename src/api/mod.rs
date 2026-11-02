use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{params, OptionalExtension};
use serde::de::DeserializeOwned;

use crate::auth::{
    self, Identity, IdentityKind, Scope, SESSION_LAST_SEEN_INTERVAL_MILLIS,
    TOKEN_LAST_USED_INTERVAL_MILLIS,
};
use crate::ids;
use crate::server::AppState;

mod error;
mod session;

pub use error::ApiError;

/// The authenticated caller, extracted from a bearer token or session cookie.
pub struct Auth(pub Identity);

impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(value) = parts.headers.get(header::AUTHORIZATION) {
            let value = value
                .to_str()
                .map_err(|_| ApiError::unauthorized("Authorization header must be valid UTF-8"))?;
            let (scheme, token) = value
                .split_once(' ')
                .ok_or_else(|| ApiError::unauthorized("unsupported authorization scheme"))?;
            if !scheme.eq_ignore_ascii_case("bearer") {
                return Err(ApiError::unauthorized("unsupported authorization scheme"));
            }
            let identity = token_identity(state, token.trim()).await?;
            return Ok(Self(identity));
        }

        let Some(cookie) = auth::session_cookie_value(&parts.headers) else {
            return Err(ApiError::unauthorized("authentication required"));
        };
        let Some(session_id) = state.auth.verify_session(&cookie) else {
            return Err(
                ApiError::unauthorized("invalid session").clear_cookie(state.auth.clear_cookie())
            );
        };

        let now = now_millis();
        let session_id_for_query = session_id.clone();
        let valid = state
            .db
            .call(move |connection| {
                let row = connection
                    .query_row(
                        "SELECT expires_at, last_seen FROM ui_sessions WHERE id = ?1",
                        [&session_id_for_query],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()?;
                match row {
                    Some((expires_at, last_seen)) if expires_at > now => {
                        if now - last_seen >= SESSION_LAST_SEEN_INTERVAL_MILLIS {
                            connection.execute(
                                "UPDATE ui_sessions SET last_seen = ?1 WHERE id = ?2",
                                params![now, session_id_for_query],
                            )?;
                        }
                        Ok(true)
                    }
                    _ => {
                        connection.execute(
                            "DELETE FROM ui_sessions WHERE id = ?1",
                            [&session_id_for_query],
                        )?;
                        Ok(false)
                    }
                }
            })
            .await?;

        if !valid {
            return Err(
                ApiError::unauthorized("session expired").clear_cookie(state.auth.clear_cookie())
            );
        }

        if auth::is_mutating(&parts.method)
            && !auth::csrf_origin_allowed(&parts.headers, state.auth.public_url())
        {
            return Err(ApiError::csrf());
        }

        Ok(Self(Identity {
            kind: IdentityKind::Ui,
            name: "ui".to_owned(),
            scope: Scope::Human,
            session_id: Some(session_id),
        }))
    }
}

async fn token_identity(state: &AppState, token: &str) -> Result<Identity, ApiError> {
    if !token.starts_with("kanso_") {
        return Err(ApiError::unauthorized("invalid token"));
    }
    let hash = ids::sha256_hex(token);
    let now = now_millis();
    let row = state
        .db
        .call(move |connection| {
            let row = connection
                .query_row(
                    "SELECT id, name, scope, last_used_at FROM tokens
                     WHERE hash = ?1 AND revoked_at IS NULL",
                    [&hash],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((id, name, scope, last_used)) = row {
                let stale = match last_used {
                    Some(last_used) => now - last_used >= TOKEN_LAST_USED_INTERVAL_MILLIS,
                    None => true,
                };
                if stale {
                    connection.execute(
                        "UPDATE tokens SET last_used_at = ?1 WHERE id = ?2",
                        params![now, id],
                    )?;
                }
                Ok(Some((name, scope)))
            } else {
                Ok(None)
            }
        })
        .await?;

    match row {
        Some((name, scope)) => Ok(Identity {
            kind: IdentityKind::Token,
            name,
            scope: Scope::parse(&scope).unwrap_or(Scope::Agent),
            session_id: None,
        }),
        None => Err(ApiError::unauthorized("invalid or revoked token")),
    }
}

/// JSON body extractor with the API error envelope on malformed input.
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(ApiError::rejection(
                rejection.status(),
                format!("invalid JSON body: {}", rejection.body_text()),
            )),
        }
    }
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Reject an agent token for a human-only action.
pub fn require_human(identity: &Identity) -> Result<(), ApiError> {
    if identity.is_human() {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "this action requires a human token or a UI session",
        ))
    }
}

pub fn response_with_cookie(status: axum::http::StatusCode, cookie: &str) -> Response {
    let mut response = status.into_response();
    if let Ok(value) = HeaderValue::from_str(cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/auth/login", post(session::login))
        .route("/api/auth/logout", post(session::logout))
        .route("/api/me", get(session::me))
}
