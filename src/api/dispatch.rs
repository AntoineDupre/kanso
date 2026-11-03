use axum::extract::{Path, State};
use axum::Json;
use rusqlite::params;
use serde_json::json;

use super::model::{Project, Task, TaskDetail};
use super::projects::project_by_ref;
use super::tasks::{insert_activity, task_by_ref, task_detail};
use super::{now_millis, require_human, ApiError, Auth};
use crate::adapter::openchamber::{
    build_session_create, CreatedSession, OpenChamberError, DISPATCH_TIMEOUT,
};
use crate::rank;
use crate::server::AppState;

enum Loaded {
    Found(Box<Project>, Box<Task>),
    NotFound,
}

enum Stored {
    Saved(Box<TaskDetail>),
    NotFound,
    Claimed,
    Changed,
}

/// Releases the in-memory dispatch reservation even on an early return.
struct DispatchGuard {
    state: AppState,
    task_id: String,
}

impl Drop for DispatchGuard {
    fn drop(&mut self) {
        self.state.release_dispatch(&self.task_id);
    }
}

/// `POST /api/tasks/:ref/dispatch` — human only.
///
/// The upstream call can take minutes for a worktree, so it runs outside the
/// database lock: read the card, release SQLite, call OpenChamber, then write
/// the result back in one short transaction. A failed auto-accept never undoes
/// the dispatch.
pub async fn dispatch(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
) -> Result<Json<TaskDetail>, ApiError> {
    require_human(&auth.0)?;
    let openchamber = state.openchamber.clone().ok_or_else(|| {
        ApiError::service_unavailable(
            "integration_disabled",
            "OpenChamber integration is disabled; set KANSO_OPENCHAMBER_URL and KANSO_OPENCHAMBER_TOKEN",
        )
    })?;

    let loaded = state
        .db
        .call(move |connection| {
            let Some(task) = task_by_ref(connection, &reference)? else {
                return Ok(Loaded::NotFound);
            };
            let Some(project) = project_by_ref(connection, &task.project_id)? else {
                return Ok(Loaded::NotFound);
            };
            Ok(Loaded::Found(Box::new(project), Box::new(task)))
        })
        .await?;
    let (project, task) = match loaded {
        Loaded::Found(project, task) => (*project, *task),
        Loaded::NotFound => return Err(ApiError::not_found("task not found")),
    };

    if task.state != "active" {
        return Err(ApiError::conflict_code(
            "not_active",
            "only active cards can be dispatched",
        ));
    }
    if task.session_id.is_some()
        && matches!(task.session_status.as_deref(), Some("running" | "waiting"))
    {
        return Err(ApiError::conflict_code(
            "already_dispatched",
            "card already has a running session",
        ));
    }
    if let Some(claimed_by) = task.claimed_by.as_deref() {
        return Err(ApiError::conflict_code(
            "already_claimed",
            format!("card is already claimed by {claimed_by}"),
        ));
    }
    if project
        .repo_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .is_none()
    {
        return Err(ApiError::validation(
            "the project has no repo_path, so no worktree can be created",
        ));
    }

    if !state.reserve_dispatch(&task.id) {
        return Err(ApiError::conflict_code(
            "already_dispatched",
            "a dispatch is already in progress for this card",
        ));
    }
    let _guard = DispatchGuard {
        state: state.clone(),
        task_id: task.id.clone(),
    };

    let payload = build_session_create(&project, &task);
    let created = openchamber
        .create_session(payload, DISPATCH_TIMEOUT)
        .await
        .map_err(|error| map_error(&error))?;
    let CreatedSession {
        session_id,
        directory,
    } = created;

    let actor = auth.0.actor().to_owned();
    let task_id = task.id.clone();
    let expected_status = task.status.clone();
    let expected_session_id = task.session_id.clone();
    let stored = match store_dispatch(
        &state,
        task_id.clone(),
        expected_status,
        expected_session_id,
        actor,
        session_id.clone(),
        directory.clone(),
    )
    .await
    {
        Ok(stored) => stored,
        Err(error) => {
            tracing::warn!(
                error = ?error,
                session_id = %session_id,
                directory = %directory,
                "could not store the OpenChamber dispatch"
            );
            record_abandoned_session(&state, &task_id, &session_id, &directory, "store_failed")
                .await;
            return Err(error);
        }
    };
    let detail = match stored {
        Stored::Saved(detail) => detail,
        Stored::NotFound => return Err(ApiError::not_found("task not found")),
        Stored::Claimed => {
            tracing::warn!(
                session_id = %session_id,
                "OpenChamber session created but the card was claimed before the result was stored"
            );
            record_abandoned_session(&state, &task_id, &session_id, &directory, "claimed").await;
            return Err(ApiError::conflict_code(
                "already_claimed",
                "card was claimed while the session was being created",
            ));
        }
        Stored::Changed => {
            tracing::warn!(
                session_id = %session_id,
                "OpenChamber session created but the card changed before the result was stored"
            );
            record_abandoned_session(&state, &task_id, &session_id, &directory, "card_changed")
                .await;
            return Err(ApiError::conflict_code(
                "conflict",
                "card changed while the OpenChamber session was being created",
            ));
        }
    };
    state.publish("task.updated", json!({ "task": &detail.task }));

    // Step 4 is best-effort: losing auto-accept is visible in the activity log,
    // but it must never lose the dispatch itself.
    if let Err(error) = openchamber.auto_accept(&session_id, &directory).await {
        tracing::warn!(
            %error,
            session_id = %session_id,
            directory = %directory,
            "OpenChamber auto-accept failed; keeping the dispatch"
        );
        match record_auto_accept_failure(&state, &detail, &session_id, &error).await {
            Ok(updated) => {
                state.publish("task.updated", json!({ "task": &updated.task }));
                return Ok(Json(*updated));
            }
            Err(record_error) => {
                tracing::warn!(%record_error, "could not record the auto-accept failure");
            }
        }
    }

    Ok(Json(*detail))
}

fn map_error(error: &OpenChamberError) -> ApiError {
    match error {
        OpenChamberError::Http { status, message } => {
            ApiError::bad_gateway(format!("OpenChamber returned HTTP {status}: {message}"))
        }
        OpenChamberError::Transport(message) => ApiError::service_unavailable(
            "integration_unreachable",
            format!("OpenChamber is unreachable: {message}"),
        ),
        OpenChamberError::Protocol(message) => ApiError::bad_gateway(format!(
            "OpenChamber answered with an invalid response: {message}"
        )),
    }
}

async fn store_dispatch(
    state: &AppState,
    task_id: String,
    expected_status: String,
    expected_session_id: Option<String>,
    actor: String,
    session_id: String,
    directory: String,
) -> Result<Stored, ApiError> {
    Ok(state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(task) = task_by_ref(&transaction, &task_id)? else {
                return Ok(Stored::NotFound);
            };
            if task.claimed_by.is_some() {
                return Ok(Stored::Claimed);
            }
            // The upstream call can take minutes: never override a move,
            // archive or claim the human made while it was running.
            if task.state != "active"
                || task.status != expected_status
                || task.session_id != expected_session_id
            {
                return Ok(Stored::Changed);
            }

            let from = task.status.clone();
            let rank = if task.status == "in_progress" {
                None
            } else {
                let max_rank: Option<f64> = transaction.query_row(
                    "SELECT MAX(rank) FROM tasks
                     WHERE project_id = ?1 AND status = 'in_progress' AND state = 'active'",
                    [&task.project_id],
                    |row| row.get(0),
                )?;
                Some(rank::append(max_rank))
            };
            let now = now_millis();
            transaction.execute(
                "UPDATE tasks
                 SET session_id = ?1, session_directory = ?2, session_status = 'running',
                     claimed_by = 'openchamber', status = 'in_progress',
                     rank = COALESCE(?3, rank), attention = 0,
                     done_at = NULL, blocked_from = NULL, blocked_reason = NULL,
                     updated_at = ?4
                 WHERE id = ?5 AND claimed_by IS NULL",
                params![session_id, directory, rank, now, task.id],
            )?;
            insert_activity(
                &transaction,
                &task.id,
                &actor,
                "dispatched",
                &json!({
                    "session_id": session_id,
                    "directory": directory,
                    "from": from,
                    "to": "in_progress",
                }),
            )?;
            let task = task_by_ref(&transaction, &task.id)?
                .ok_or_else(|| crate::error::Error::msg("dispatched task missing"))?;
            let detail = task_detail(&transaction, task)?;
            transaction.commit()?;
            Ok(Stored::Saved(Box::new(detail)))
        })
        .await?)
}

/// The upstream session exists but the card changed underneath it. Leave an
/// audit trail so the orphaned session/worktree can be found and cleaned up.
async fn record_abandoned_session(
    state: &AppState,
    task_id: &str,
    session_id: &str,
    directory: &str,
    reason: &str,
) {
    let task_id = task_id.to_owned();
    let task_id_for_log = task_id.clone();
    let session_id = session_id.to_owned();
    let directory = directory.to_owned();
    let reason = reason.to_owned();
    if let Err(error) = state
        .db
        .call(move |connection| {
            insert_activity(
                connection,
                &task_id,
                "openchamber",
                "dispatch_abandoned",
                &json!({
                    "session_id": session_id,
                    "directory": directory,
                    "reason": reason,
                }),
            )
        })
        .await
    {
        tracing::warn!(
            %error,
            task_id = %task_id_for_log,
            "could not record the abandoned OpenChamber session"
        );
    }
}

async fn record_auto_accept_failure(
    state: &AppState,
    detail: &TaskDetail,
    session_id: &str,
    error: &OpenChamberError,
) -> Result<Box<TaskDetail>, crate::error::Error> {
    let task_id = detail.task.id.clone();
    let session_id = session_id.to_owned();
    let message: String = error.to_string().chars().take(500).collect();
    state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            insert_activity(
                &transaction,
                &task_id,
                "openchamber",
                "permission_auto_accept_failed",
                &json!({ "session_id": session_id, "error": message }),
            )?;
            transaction.execute(
                "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
                params![now_millis(), task_id],
            )?;
            let task = task_by_ref(&transaction, &task_id)?
                .ok_or_else(|| crate::error::Error::msg("dispatched task missing"))?;
            let detail = task_detail(&transaction, task)?;
            transaction.commit()?;
            Ok(Box::new(detail))
        })
        .await
}
