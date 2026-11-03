use axum::extract::{Path, State};
use axum::Json;
use rusqlite::params;
use serde::Deserialize;
use serde_json::json;

use super::model::{Task, TaskDetail, TASK_COLUMNS};
use super::tasks::{insert_activity, load_labels, task_by_ref, task_detail, validate_status};
use super::{now_millis, require_human, ApiError, ApiJson, Auth};
use crate::rank;
use crate::server::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptProposal {
    status: Option<String>,
}

pub async fn list(State(state): State<AppState>, _auth: Auth) -> Result<Json<Vec<Task>>, ApiError> {
    let tasks = state
        .db
        .call(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM tasks t JOIN projects p ON p.id = t.project_id
                 WHERE t.state = 'proposed'
                 ORDER BY t.created_at DESC, t.id DESC"
            ))?;
            let mut tasks = statement
                .query_map([], Task::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            for task in &mut tasks {
                task.labels = load_labels(connection, &task.id)?;
            }
            Ok(tasks)
        })
        .await?;
    Ok(Json(tasks))
}

enum AcceptOutcome {
    Accepted(Box<TaskDetail>),
    NotFound,
    NotProposed,
}

pub async fn accept(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
    ApiJson(payload): ApiJson<AcceptProposal>,
) -> Result<Json<TaskDetail>, ApiError> {
    require_human(&auth.0)?;
    let status = match payload.status.as_deref() {
        Some(status) => validate_status(status)?,
        None => "backlog".to_owned(),
    };
    let actor = auth.0.actor().to_owned();
    let now = now_millis();

    let outcome = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(task) = task_by_ref(&transaction, &reference)? else {
                return Ok(AcceptOutcome::NotFound);
            };
            if task.state != "proposed" {
                return Ok(AcceptOutcome::NotProposed);
            }
            let max_rank: Option<f64> = transaction.query_row(
                "SELECT MAX(rank) FROM tasks WHERE project_id = ?1 AND status = ?2",
                params![task.project_id, status],
                |row| row.get(0),
            )?;
            let rank = rank::append(max_rank);
            let done_at = (status == "done").then_some(now);
            // A proposal always starts in backlog; accepting straight into
            // blocker still needs a column to restore to.
            let blocked_from = (status == "blocker").then_some(task.status.clone());
            transaction.execute(
                "UPDATE tasks
                 SET state = 'active', status = ?1, rank = ?2, archived_at = NULL,
                     blocked_from = ?3, blocked_reason = NULL, done_at = ?4, updated_at = ?5
                 WHERE id = ?6",
                params![status, rank, blocked_from, done_at, now, task.id],
            )?;
            insert_activity(
                &transaction,
                &task.id,
                &actor,
                "accepted",
                &json!({ "status": status }),
            )?;
            let task = task_by_ref(&transaction, &task.id)?
                .ok_or_else(|| crate::error::Error::msg("accepted task missing"))?;
            let detail = task_detail(&transaction, task)?;
            transaction.commit()?;
            Ok(AcceptOutcome::Accepted(Box::new(detail)))
        })
        .await?;

    match outcome {
        AcceptOutcome::Accepted(detail) => {
            state.publish("proposal.accepted", json!({ "task": &detail.task }));
            Ok(Json(*detail))
        }
        AcceptOutcome::NotFound => Err(ApiError::not_found("proposal not found")),
        AcceptOutcome::NotProposed => Err(ApiError::conflict_code(
            "not_proposed",
            "card is not a proposal",
        )),
    }
}

enum RejectOutcome {
    Rejected(Box<TaskDetail>),
    NotFound,
    NotProposed,
}

pub async fn reject(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
) -> Result<Json<TaskDetail>, ApiError> {
    require_human(&auth.0)?;
    let actor = auth.0.actor().to_owned();
    let now = now_millis();

    let outcome = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(task) = task_by_ref(&transaction, &reference)? else {
                return Ok(RejectOutcome::NotFound);
            };
            if task.state != "proposed" {
                return Ok(RejectOutcome::NotProposed);
            }
            transaction.execute(
                "UPDATE tasks
                 SET state = 'archived', archived_at = ?1, updated_at = ?1
                 WHERE id = ?2",
                params![now, task.id],
            )?;
            insert_activity(&transaction, &task.id, &actor, "rejected", &json!({}))?;
            let task = task_by_ref(&transaction, &task.id)?
                .ok_or_else(|| crate::error::Error::msg("rejected task missing"))?;
            let detail = task_detail(&transaction, task)?;
            transaction.commit()?;
            Ok(RejectOutcome::Rejected(Box::new(detail)))
        })
        .await?;

    match outcome {
        RejectOutcome::Rejected(detail) => {
            state.publish("proposal.rejected", json!({ "task": &detail.task }));
            Ok(Json(*detail))
        }
        RejectOutcome::NotFound => Err(ApiError::not_found("proposal not found")),
        RejectOutcome::NotProposed => Err(ApiError::conflict_code(
            "not_proposed",
            "card is not a proposal",
        )),
    }
}
