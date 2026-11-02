use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::params;
use serde::Deserialize;
use serde_json::json;

use super::model::Comment;
use super::tasks::{insert_activity, task_by_ref};
use super::{now_millis, ApiError, ApiJson, Auth};
use crate::ids;
use crate::server::AppState;

const MAX_COMMENT_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateComment {
    body: String,
}

fn validate_body(raw: &str) -> Result<String, ApiError> {
    if raw.trim().is_empty() {
        return Err(ApiError::validation("comment body must not be empty"));
    }
    if raw.len() > MAX_COMMENT_BYTES {
        return Err(ApiError::validation("comment body must be at most 16 KiB"));
    }
    Ok(raw.to_owned())
}

pub async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
    ApiJson(payload): ApiJson<CreateComment>,
) -> Result<Response, ApiError> {
    let body = validate_body(&payload.body)?;
    let author = auth.0.actor().to_owned();
    let id = ids::uuid_v4()?;
    let now = now_millis();

    let comment = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(task) = task_by_ref(&transaction, &reference)? else {
                return Ok(None);
            };
            transaction.execute(
                "INSERT INTO comments (id, task_id, author, body, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, task.id, author, body, now],
            )?;
            insert_activity(
                &transaction,
                &task.id,
                &author,
                "commented",
                &json!({ "comment_id": id }),
            )?;
            transaction.commit()?;
            Ok(Some(Comment {
                id,
                author,
                body,
                created_at: now,
            }))
        })
        .await?
        .ok_or_else(|| ApiError::not_found("task not found"))?;
    Ok((StatusCode::CREATED, Json(comment)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_bodies_are_bounded() {
        assert_eq!(validate_body("hello").expect("body"), "hello");
        assert!(validate_body("   ").is_err());
        assert!(validate_body(&"x".repeat(MAX_COMMENT_BYTES)).is_ok());
        assert!(validate_body(&"x".repeat(MAX_COMMENT_BYTES + 1)).is_err());
    }
}
