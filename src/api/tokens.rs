use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::model::TokenInfo;
use super::{now_millis, require_human, ApiError, ApiJson, Auth};
use crate::auth::Scope;
use crate::ids;
use crate::server::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateToken {
    name: String,
    scope: String,
}

#[derive(Serialize)]
struct CreatedToken {
    #[serde(flatten)]
    info: TokenInfo,
    token: String,
}

fn validate_name(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(ApiError::validation("token name must not be empty"));
    }
    if name.chars().count() > 64 {
        return Err(ApiError::validation(
            "token name must be at most 64 characters",
        ));
    }
    if name.eq_ignore_ascii_case("ui") {
        return Err(ApiError::validation(
            "token name \"ui\" is reserved for the browser session",
        ));
    }
    Ok(name.to_owned())
}

fn token_info(
    connection: &rusqlite::Connection,
    id: &str,
) -> crate::error::Result<Option<TokenInfo>> {
    connection
        .query_row(
            "SELECT id, name, scope, created_at, last_used_at, revoked_at
             FROM tokens WHERE id = ?1",
            [id],
            |row| {
                Ok(TokenInfo {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    scope: row.get(2)?,
                    created_at: row.get(3)?,
                    last_used_at: row.get(4)?,
                    revoked_at: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

pub async fn list(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<TokenInfo>>, ApiError> {
    require_human(&auth.0)?;
    let tokens = state
        .db
        .call(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, name, scope, created_at, last_used_at, revoked_at
                 FROM tokens ORDER BY created_at, id",
            )?;
            let tokens = statement
                .query_map([], |row| {
                    Ok(TokenInfo {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        scope: row.get(2)?,
                        created_at: row.get(3)?,
                        last_used_at: row.get(4)?,
                        revoked_at: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(tokens)
        })
        .await?;
    Ok(Json(tokens))
}

pub async fn create(
    State(state): State<AppState>,
    auth: Auth,
    ApiJson(payload): ApiJson<CreateToken>,
) -> Result<Response, ApiError> {
    require_human(&auth.0)?;
    let name = validate_name(&payload.name)?;
    let scope = Scope::parse(payload.scope.trim())
        .ok_or_else(|| ApiError::validation("scope must be human or agent"))?;
    let id = ids::uuid_v4()?;
    let token = ids::api_token()?;
    let hash = ids::sha256_hex(&token);
    let now = now_millis();

    let info = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let duplicate = transaction
                .query_row(
                    "SELECT 1 FROM tokens WHERE name = ?1 COLLATE NOCASE",
                    [&name],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if duplicate {
                return Ok(None);
            }
            transaction.execute(
                "INSERT INTO tokens (id, name, scope, hash, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, name, scope.as_str(), hash, now],
            )?;
            let info = token_info(&transaction, &id)?;
            transaction.commit()?;
            Ok(info)
        })
        .await?
        .ok_or_else(|| ApiError::conflict("a token with this name already exists"))?;

    Ok((StatusCode::CREATED, Json(CreatedToken { info, token })).into_response())
}

pub async fn revoke(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    require_human(&auth.0)?;
    let now = now_millis();
    let found = state
        .db
        .call(move |connection| {
            let found = connection
                .query_row("SELECT 1 FROM tokens WHERE id = ?1", [&id], |_| Ok(()))
                .optional()?
                .is_some();
            if !found {
                return Ok(false);
            }
            connection.execute(
                "UPDATE tokens SET revoked_at = COALESCE(revoked_at, ?1) WHERE id = ?2",
                params![now, id],
            )?;
            Ok(true)
        })
        .await?;
    if found {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(ApiError::not_found("token not found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_names_are_bounded() {
        assert_eq!(validate_name(" build ").expect("name"), "build");
        assert!(validate_name("").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
        assert!(validate_name("ui").is_err());
        assert!(validate_name("UI").is_err());
    }
}
