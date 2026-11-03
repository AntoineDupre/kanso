use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Query, Request};
use axum::http::request::Parts;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{params, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::auth::{
    self, Identity, IdentityKind, Scope, SESSION_LAST_SEEN_INTERVAL_MILLIS,
    TOKEN_LAST_USED_INTERVAL_MILLIS,
};
use crate::ids;
use crate::server::AppState;

mod agent;
mod board;
mod checklist;
mod comments;
mod dependencies;
mod error;
mod events;
mod inbox;
mod labels;
mod model;
mod projects;
mod session;
mod tasks;
mod tokens;

pub use error::ApiError;

/// Optional peer address, present when serving with `ConnectInfo`.
pub struct PeerAddr(pub Option<SocketAddr>);

impl FromRequestParts<AppState> for PeerAddr {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|peer| peer.0),
        ))
    }
}

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
            token_id: None,
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
                Ok(Some((id, name, scope)))
            } else {
                Ok(None)
            }
        })
        .await?;

    match row {
        Some((id, name, scope)) => Ok(Identity {
            kind: IdentityKind::Token,
            name,
            scope: Scope::parse(&scope).unwrap_or(Scope::Agent),
            session_id: None,
            token_id: Some(id),
        }),
        None => Err(ApiError::unauthorized("invalid or revoked token")),
    }
}

/// Query-string extractor with the API error envelope on malformed input.
pub struct ApiQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(ApiError::validation(format!(
                "invalid query parameters: {}",
                rejection.body_text()
            ))),
        }
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
        .route("/api/events", get(events::stream))
        .route("/api/projects", get(projects::list).post(projects::create))
        .route(
            "/api/projects/{id}",
            get(projects::get).patch(projects::update),
        )
        .route("/api/projects/{id}/archive", post(projects::archive))
        .route("/api/tasks", get(tasks::list).post(tasks::create))
        .route(
            "/api/tasks/{reference}",
            get(tasks::get).patch(tasks::update).delete(tasks::delete),
        )
        .route("/api/tasks/{reference}/archive", post(tasks::archive))
        .route("/api/tasks/{reference}/restore", post(tasks::restore))
        .route("/api/tasks/{reference}/move", post(tasks::move_task))
        .route("/api/tasks/{reference}/claim", post(tasks::claim))
        .route("/api/tasks/{reference}/release", post(tasks::release))
        .route(
            "/api/tasks/{reference}/dependencies",
            post(dependencies::set),
        )
        .route("/api/agent/next", get(agent::next))
        .route("/api/inbox", get(inbox::list))
        .route("/api/inbox/{reference}/accept", post(inbox::accept))
        .route("/api/inbox/{reference}/reject", post(inbox::reject))
        .route("/api/board", get(board::global))
        .route("/api/projects/{id}/board", get(board::project_board))
        .route("/api/tasks/{reference}/comment", post(comments::create))
        .route("/api/tasks/{reference}/checklist", post(checklist::create))
        .route(
            "/api/tasks/{reference}/checklist/{item}",
            axum::routing::patch(checklist::update).delete(checklist::delete),
        )
        .route("/api/labels", get(labels::list).post(labels::create))
        .route(
            "/api/labels/{id}",
            axum::routing::patch(labels::update).delete(labels::delete),
        )
        .route("/api/tokens", get(tokens::list).post(tokens::create))
        .route("/api/tokens/{id}", axum::routing::delete(tokens::revoke))
}

/// Double-option field: absent -> `None`, `null` -> `Some(None)`, value ->
/// `Some(Some(value))`. Used by PATCH payloads that can clear nullable columns.
pub fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

pub fn parse_flag(value: Option<&str>, default: bool) -> Result<bool, ApiError> {
    match value {
        None => Ok(default),
        Some("") | Some("1") | Some("true") | Some("yes") => Ok(true),
        Some("0") | Some("false") | Some("no") => Ok(false),
        Some(other) => Err(ApiError::validation(format!(
            "invalid boolean value {other:?}; use 1 or 0"
        ))),
    }
}

/// Validate `#rrggbb` colours shared by projects and labels.
pub fn validate_color(raw: &str) -> Result<String, ApiError> {
    let color = raw.trim();
    if color.len() != 7
        || !color.starts_with('#')
        || !color[1..]
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(ApiError::validation(
            "color must be a #rrggbb hexadecimal value",
        ));
    }
    Ok(color.to_ascii_lowercase())
}
