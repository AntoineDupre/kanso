use axum::extract::{Path, State};
use axum::Json;
use rusqlite::params;
use serde::Deserialize;
use serde_json::json;

use super::model::TaskDetail;
use super::tasks::{insert_activity, task_by_ref, task_detail};
use super::{require_human, ApiError, ApiJson, Auth};
use crate::dependencies;
use crate::server::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetDependencies {
    blocked_by: Vec<String>,
}

enum SetOutcome {
    Updated(Box<TaskDetail>),
    NotFound,
    UnknownReference(String),
    SelfDependency,
    Cycle,
}

pub async fn set(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
    ApiJson(payload): ApiJson<SetDependencies>,
) -> Result<Json<TaskDetail>, ApiError> {
    require_human(&auth.0)?;
    let actor = auth.0.actor().to_owned();
    let now = super::now_millis();

    let outcome = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(task) = task_by_ref(&transaction, &reference)? else {
                return Ok(SetOutcome::NotFound);
            };

            let mut resolved: Vec<(String, String)> = Vec::new();
            for raw in &payload.blocked_by {
                let Some(dependency) = task_by_ref(&transaction, raw)? else {
                    return Ok(SetOutcome::UnknownReference(raw.clone()));
                };
                if dependency.id == task.id {
                    return Ok(SetOutcome::SelfDependency);
                }
                if !resolved.iter().any(|(id, _)| id == &dependency.id) {
                    resolved.push((dependency.id, dependency.reference));
                }
            }

            let edges = {
                let mut statement = transaction
                    .prepare("SELECT task_id, blocked_by FROM dependencies WHERE task_id <> ?1")?;
                let rows = statement.query_map([&task.id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let new_ids: Vec<String> = resolved.iter().map(|(id, _)| id.clone()).collect();
            if dependencies::creates_cycle(&edges, &task.id, &new_ids) {
                return Ok(SetOutcome::Cycle);
            }

            transaction.execute("DELETE FROM dependencies WHERE task_id = ?1", [&task.id])?;
            for dependency_id in &new_ids {
                transaction.execute(
                    "INSERT INTO dependencies (task_id, blocked_by) VALUES (?1, ?2)",
                    params![task.id, dependency_id],
                )?;
            }
            let references: Vec<&str> = resolved
                .iter()
                .map(|(_, reference)| reference.as_str())
                .collect();
            insert_activity(
                &transaction,
                &task.id,
                &actor,
                "dependencies_changed",
                &json!({ "blocked_by": references }),
            )?;
            transaction.execute(
                "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
                params![now, task.id],
            )?;

            let task = task_by_ref(&transaction, &task.id)?
                .ok_or_else(|| crate::error::Error::msg("task missing after dependencies"))?;
            let detail = task_detail(&transaction, task)?;
            transaction.commit()?;
            Ok(SetOutcome::Updated(Box::new(detail)))
        })
        .await?;

    match outcome {
        SetOutcome::Updated(detail) => {
            state.publish(
                "dependencies.changed",
                json!({ "task": &detail.task, "dependencies": &detail.dependencies }),
            );
            Ok(Json(*detail))
        }
        SetOutcome::NotFound => Err(ApiError::not_found("task not found")),
        SetOutcome::UnknownReference(reference) => {
            Err(ApiError::not_found(format!("task {reference:?} not found")))
        }
        SetOutcome::SelfDependency => Err(ApiError::validation("a card cannot depend on itself")),
        SetOutcome::Cycle => Err(ApiError::validation(
            "these dependencies would create a cycle",
        )),
    }
}
