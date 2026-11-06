//! Human replies to a blocked agent: an ordinary comment that is forwarded into
//! the card's OpenChamber session, with the delivery attempt recorded on it.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::comments::{by_id, validate_body};
use super::model::Comment;
use super::tasks::{insert_activity, task_by_ref};
use super::{now_millis, require_human, ApiError, ApiJson, Auth};
use crate::adapter::openchamber::{build_reply_prompt, OpenChamber, PromptDelivery, SEND_TIMEOUT};
use crate::ids;
use crate::server::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyBody {
    body: String,
}

#[derive(Serialize)]
pub(super) struct Delivery {
    status: &'static str,
    session_id: Option<String>,
    error: Option<String>,
}

impl Delivery {
    fn from_outcome(outcome: &PromptDelivery, session_id: &str) -> Self {
        let (status, error) = match outcome {
            PromptDelivery::Sent => ("sent", None),
            PromptDelivery::Failed(error) => ("failed", Some(error.clone())),
            PromptDelivery::Unknown(error) => ("unknown", Some(error.clone())),
        };
        Self {
            status,
            session_id: Some(session_id.to_owned()),
            error,
        }
    }

    fn activity_kind(&self) -> &'static str {
        match self.status {
            "sent" => "reply_sent",
            "failed" => "reply_failed",
            _ => "reply_unknown",
        }
    }
}

#[derive(Serialize)]
pub(super) struct ReplyResponse {
    comment: Comment,
    delivery: Delivery,
}

struct InsertReply {
    task_id: String,
    comment_id: String,
    author: String,
    body: String,
    prompt: String,
    session_id: String,
    directory: Option<String>,
    now: i64,
}

struct DeliveryRequest {
    task_id: String,
    comment_id: String,
    session_id: String,
    directory: Option<String>,
    prompt: String,
    actor: String,
}

struct StoredReply {
    prompt: Option<String>,
    status: Option<String>,
    session_id: Option<String>,
    directory: Option<String>,
}

fn integration_disabled() -> ApiError {
    ApiError::service_unavailable(
        "integration_disabled",
        "OpenChamber integration is disabled; set KANSO_OPENCHAMBER_URL and KANSO_OPENCHAMBER_TOKEN",
    )
}

fn reply_not_active() -> ApiError {
    ApiError::conflict_code("not_active", "only active cards can be answered")
}

fn reply_not_blocked() -> ApiError {
    ApiError::conflict_code(
        "not_blocked",
        "the card is not in blocker, so there is no agent question",
    )
}

fn reply_no_session() -> ApiError {
    ApiError::conflict_code(
        "no_session",
        "the card has no agent session to deliver a reply to",
    )
}

/// `POST /api/tasks/:ref/reply` — human only.
///
/// The comment and its `commented` activity commit before the upstream call, so
/// a delivery failure never loses the human's answer.
pub async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
    ApiJson(payload): ApiJson<ReplyBody>,
) -> Result<Response, ApiError> {
    require_human(&auth.0)?;
    let body = validate_body(&payload.body)?;

    let task = state
        .db
        .call(move |connection| task_by_ref(connection, &reference))
        .await?
        .ok_or_else(|| ApiError::not_found("task not found"))?;
    if task.state != "active" {
        return Err(reply_not_active());
    }
    if task.status != "blocker" {
        return Err(reply_not_blocked());
    }
    let Some(session_id) = task.session_id.clone() else {
        return Err(reply_no_session());
    };
    let openchamber = state.openchamber.clone().ok_or_else(integration_disabled)?;

    let comment_id = ids::uuid_v4()?;
    let author = auth.0.actor().to_owned();
    let directory = task.session_directory.clone();
    let prompt = build_reply_prompt(
        &task.reference,
        &task.title,
        &comment_id,
        task.blocked_reason.as_deref(),
        &body,
    );

    let insert = InsertReply {
        task_id: task.id.clone(),
        comment_id: comment_id.clone(),
        author: author.clone(),
        body,
        prompt: prompt.clone(),
        session_id: session_id.clone(),
        directory: directory.clone(),
        now: now_millis(),
    };
    state
        .db
        .call(move |connection| insert_reply(connection, insert))
        .await?;

    let request = DeliveryRequest {
        task_id: task.id.clone(),
        comment_id,
        session_id,
        directory,
        prompt,
        actor: author,
    };
    let (comment, delivery) = deliver(&state, &openchamber, request).await?;
    state.publish(
        "comment.created",
        json!({ "task_id": task.id, "comment": &comment }),
    );
    Ok((
        StatusCode::CREATED,
        Json(ReplyResponse { comment, delivery }),
    )
        .into_response())
}

/// `POST /api/tasks/:ref/reply/:comment_id` — human only.
///
/// Reuses the stored prompt and session so a failure, or an `unknown` delivery,
/// is retried without rebuilding the message.
pub async fn retry(
    State(state): State<AppState>,
    auth: Auth,
    Path((reference, comment_id)): Path<(String, String)>,
) -> Result<Json<ReplyResponse>, ApiError> {
    require_human(&auth.0)?;
    let comment_id_for_db = comment_id.clone();
    let loaded = state
        .db
        .call(move |connection| {
            let Some(task) = task_by_ref(connection, &reference)? else {
                return Ok(None);
            };
            let reply = stored_reply(connection, &task.id, &comment_id_for_db)?;
            Ok(Some((task, reply)))
        })
        .await?;
    let (task, reply) = loaded.ok_or_else(|| ApiError::not_found("task not found"))?;
    let Some(reply) = reply else {
        return Err(ApiError::not_found("comment not found"));
    };
    if task.state != "active" {
        return Err(reply_not_active());
    }
    if reply.status.as_deref() == Some("sent") {
        return Err(ApiError::conflict_code(
            "already_sent",
            "this reply was already delivered",
        ));
    }
    let (Some(prompt), Some(session_id)) = (reply.prompt, reply.session_id) else {
        return Err(ApiError::conflict_code(
            "not_a_reply",
            "this comment is not a forwarded reply",
        ));
    };
    let openchamber = state.openchamber.clone().ok_or_else(integration_disabled)?;

    let request = DeliveryRequest {
        task_id: task.id.clone(),
        comment_id,
        session_id,
        directory: reply.directory,
        prompt,
        actor: auth.0.actor().to_owned(),
    };
    let (comment, delivery) = deliver(&state, &openchamber, request).await?;
    state.publish(
        "comment.updated",
        json!({ "task_id": task.id, "comment": &comment }),
    );
    Ok(Json(ReplyResponse { comment, delivery }))
}

fn insert_reply(
    connection: &mut rusqlite::Connection,
    reply: InsertReply,
) -> crate::error::Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT INTO comments
           (id, task_id, author, body, created_at, delivery_status, delivery_error,
            delivery_session_id, delivery_session_directory, reply_prompt)
         VALUES (?1, ?2, ?3, ?4, ?5, 'unknown', NULL, ?6, ?7, ?8)",
        params![
            reply.comment_id,
            reply.task_id,
            reply.author,
            reply.body,
            reply.now,
            reply.session_id,
            reply.directory,
            reply.prompt
        ],
    )?;
    insert_activity(
        &transaction,
        &reply.task_id,
        &reply.author,
        "commented",
        &json!({ "comment_id": reply.comment_id }),
    )?;
    transaction.commit()?;
    Ok(())
}

async fn deliver(
    state: &AppState,
    openchamber: &OpenChamber,
    request: DeliveryRequest,
) -> Result<(Comment, Delivery), ApiError> {
    let outcome = openchamber
        .send_prompt(
            &request.session_id,
            request.directory.as_deref().unwrap_or_default(),
            &request.prompt,
            SEND_TIMEOUT,
        )
        .await;
    let delivery = Delivery::from_outcome(&outcome, &request.session_id);
    let status = delivery.status;
    let error = delivery.error.clone();
    let kind = delivery.activity_kind();
    let now = now_millis();

    let comment = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "UPDATE comments SET delivery_status = ?1, delivery_error = ?2 WHERE id = ?3",
                params![status, error, request.comment_id],
            )?;
            if status == "sent" {
                transaction.execute(
                    "UPDATE tasks
                     SET session_status = 'running', attention = 0, updated_at = ?1
                     WHERE id = ?2",
                    params![now, request.task_id],
                )?;
            }
            let mut data = json!({
                "comment_id": request.comment_id,
                "session_id": request.session_id,
            });
            if let Some(error) = &error {
                data["error"] = json!(error);
            }
            insert_activity(&transaction, &request.task_id, &request.actor, kind, &data)?;
            let comment = by_id(&transaction, &request.comment_id)?
                .ok_or_else(|| crate::error::Error::msg("reply comment missing"))?;
            transaction.commit()?;
            Ok(comment)
        })
        .await?;
    Ok((comment, delivery))
}

fn stored_reply(
    connection: &rusqlite::Connection,
    task_id: &str,
    comment_id: &str,
) -> crate::error::Result<Option<StoredReply>> {
    connection
        .query_row(
            "SELECT reply_prompt, delivery_status, delivery_session_id, delivery_session_directory
             FROM comments WHERE id = ?1 AND task_id = ?2",
            params![comment_id, task_id],
            |row| {
                Ok(StoredReply {
                    prompt: row.get(0)?,
                    status: row.get(1)?,
                    session_id: row.get(2)?,
                    directory: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}
