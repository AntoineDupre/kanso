use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Query, Request};
use axum::http::request::Parts;
use axum::http::{header, HeaderValue, StatusCode};
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
mod backup;
mod board;
mod checklist;
mod comments;
mod dependencies;
mod dispatch;
mod error;
mod events;
mod export;
mod github_repositories;
mod imports;
mod inbox;
mod integration;
mod labels;
pub(crate) mod model;
mod projects;
mod reply;
pub(crate) mod secret;
mod session;
pub(crate) mod tasks;
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
            let mut identity = token_identity(state, token.trim()).await?;
            apply_agent_header(&mut identity, &parts.headers)?;
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
            display_name: None,
            session_id: Some(session_id),
            token_id: None,
        }))
    }
}

/// Header an agent token may use to override its display name.
///
/// It is honoured only for agent-scoped bearer tokens. A human token or UI
/// session ignores it. An agent token sending an empty, over-long, reserved or
/// control-character name is rejected with `400 validation` rather than having
/// the header silently dropped, so `--agent` mistakes surface immediately.
pub const AGENT_HEADER: &str = "x-kanso-agent";
pub const MAX_AGENT_NAME_CHARS: usize = 64;

fn apply_agent_header(
    identity: &mut Identity,
    headers: &header::HeaderMap,
) -> Result<(), ApiError> {
    if identity.scope != Scope::Agent {
        return Ok(());
    }
    let Some(value) = headers.get(AGENT_HEADER) else {
        return Ok(());
    };
    let raw = value
        .to_str()
        .map_err(|_| ApiError::validation("X-Kanso-Agent must be valid UTF-8"))?;
    let name = raw.trim();
    if name.is_empty() {
        return Err(ApiError::validation("X-Kanso-Agent must not be empty"));
    }
    if name.chars().count() > MAX_AGENT_NAME_CHARS {
        return Err(ApiError::validation(format!(
            "X-Kanso-Agent must be at most {MAX_AGENT_NAME_CHARS} characters"
        )));
    }
    if name.eq_ignore_ascii_case("ui") {
        return Err(ApiError::validation(
            "X-Kanso-Agent may not impersonate \"ui\"",
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(ApiError::validation(
            "X-Kanso-Agent must not contain control characters",
        ));
    }
    identity.display_name = Some(name.to_owned());
    Ok(())
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
            display_name: None,
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
        match <Json<T> as FromRequest<S>>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(ApiError::rejection(
                rejection.status(),
                format!("invalid JSON body: {}", rejection.body_text()),
            )),
        }
    }
}

/// Optional JSON body: an empty body is `None` whatever the content type,
/// while a non-empty body must be JSON and uses the same error envelope as
/// [`ApiJson`].
pub struct OptionalApiJson<T>(pub Option<T>);

impl<T, S> FromRequest<S> for OptionalApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let is_json = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                let mime = value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
                mime == "application/json" || mime.ends_with("+json")
            })
            .unwrap_or(false);
        let bytes = Bytes::from_request(request, state)
            .await
            .map_err(|rejection| {
                ApiError::rejection(
                    rejection.status(),
                    format!("could not read the request body: {}", rejection.body_text()),
                )
            })?;
        if bytes.is_empty() {
            return Ok(Self(None));
        }
        if !is_json {
            return Err(ApiError::rejection(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "expected a JSON request body",
            ));
        }
        let value = serde_json::from_slice(&bytes).map_err(|error| {
            ApiError::rejection(
                StatusCode::BAD_REQUEST,
                format!("invalid JSON body: {error}"),
            )
        })?;
        Ok(Self(Some(value)))
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
        .route("/api/imports/providers", get(imports::providers))
        .route("/api/imports/preview", post(imports::preview))
        .route("/api/imports/confirm", post(imports::confirm))
        .route("/api/auth/login", post(session::login))
        .route("/api/auth/logout", post(session::logout))
        .route("/api/me", get(session::me))
        .route("/api/events", get(events::stream))
        .route("/api/integration", get(integration::status))
        .route("/api/projects", get(projects::list).post(projects::create))
        .route(
            "/api/projects/{id}",
            get(projects::get).patch(projects::update),
        )
        .route("/api/projects/{id}/archive", post(projects::archive))
        .route(
            "/api/projects/{id}/github-repositories",
            get(github_repositories::list).put(github_repositories::replace),
        )
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
        .route("/api/tasks/{reference}/session", post(tasks::link_session))
        .route("/api/tasks/{reference}/dispatch", post(dispatch::dispatch))
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
        .route("/api/tasks/{reference}/reply", post(reply::create))
        .route(
            "/api/tasks/{reference}/secrets",
            post(secret::create).get(secret::list),
        )
        .route("/api/secrets/{id}", get(secret::status))
        .route("/api/secrets/{id}/submit", post(secret::submit))
        .route("/api/secrets/{id}/consume", post(secret::consume))
        .route("/api/secrets/{id}/cancel", post(secret::cancel))
        .route("/api/secrets/{id}/decline", post(secret::decline))
        .route("/api/secrets/{id}/retry", post(secret::retry))
        .route(
            "/api/tasks/{reference}/reply/{comment_id}",
            post(reply::retry),
        )
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
        .route("/api/export.json", get(export::download))
        .route("/api/backup", post(backup::create))
        .layer(axum::middleware::map_response(no_store))
}

async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
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

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(scope: Scope) -> Identity {
        Identity {
            kind: IdentityKind::Token,
            name: "build".to_owned(),
            scope,
            display_name: None,
            session_id: None,
            token_id: Some("token-id".to_owned()),
        }
    }

    fn headers(value: &str) -> header::HeaderMap {
        let mut headers = header::HeaderMap::new();
        headers.insert(AGENT_HEADER, value.parse().expect("header value"));
        headers
    }

    #[test]
    fn agent_header_overrides_only_agent_tokens() {
        let mut agent = identity(Scope::Agent);
        apply_agent_header(&mut agent, &headers("  reviewer  ")).expect("apply");
        assert_eq!(agent.actor(), "reviewer");
        assert_eq!(agent.name, "build");

        let mut human = identity(Scope::Human);
        apply_agent_header(&mut human, &headers("reviewer")).expect("ignored");
        assert_eq!(human.actor(), "build");
    }

    #[test]
    fn agent_header_rejects_empty_reserved_and_overlong_names() {
        let mut agent = identity(Scope::Agent);
        assert!(apply_agent_header(&mut agent, &headers("   ")).is_err());
        assert!(apply_agent_header(&mut agent, &headers("UI")).is_err());
        assert!(apply_agent_header(&mut agent, &headers(&"x".repeat(65))).is_err());
        assert_eq!(agent.name, "build");
    }
}
