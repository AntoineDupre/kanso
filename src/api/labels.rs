use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::json;

use super::model::Label;
use super::tasks::insert_activity;
use super::{require_human, validate_color, ApiError, ApiJson, Auth};
use crate::ids;
use crate::server::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateLabel {
    name: String,
    color: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateLabel {
    name: Option<String>,
    color: Option<String>,
}

fn validate_name(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(ApiError::validation("label name must not be empty"));
    }
    if name.chars().count() > 32 {
        return Err(ApiError::validation(
            "label name must be at most 32 characters",
        ));
    }
    Ok(name.to_owned())
}

fn label_by_id(connection: &rusqlite::Connection, id: &str) -> crate::error::Result<Option<Label>> {
    connection
        .query_row(
            "SELECT id, name, color FROM labels WHERE id = ?1",
            [id],
            |row| {
                Ok(Label {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    color: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

pub async fn list(
    State(state): State<AppState>,
    _auth: Auth,
) -> Result<Json<Vec<Label>>, ApiError> {
    let labels = state
        .db
        .call(|connection| {
            let mut statement = connection
                .prepare("SELECT id, name, color FROM labels ORDER BY name COLLATE NOCASE, id")?;
            let labels = statement
                .query_map([], |row| {
                    Ok(Label {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        color: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(labels)
        })
        .await?;
    Ok(Json(labels))
}

pub async fn create(
    State(state): State<AppState>,
    auth: Auth,
    ApiJson(payload): ApiJson<CreateLabel>,
) -> Result<Response, ApiError> {
    require_human(&auth.0)?;
    let name = validate_name(&payload.name)?;
    let color = validate_color(&payload.color)?;
    let id = ids::uuid_v4()?;

    let label = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let duplicate = transaction
                .query_row(
                    "SELECT 1 FROM labels WHERE name = ?1 COLLATE NOCASE",
                    [&name],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if duplicate {
                return Ok(None);
            }
            transaction.execute(
                "INSERT INTO labels (id, name, color) VALUES (?1, ?2, ?3)",
                params![id, name, color],
            )?;
            let label = label_by_id(&transaction, &id)?;
            transaction.commit()?;
            Ok(label)
        })
        .await?
        .ok_or_else(|| ApiError::conflict("a label with this name already exists"))?;
    state.publish("label.created", json!({ "label": &label }));
    Ok((StatusCode::CREATED, Json(label)).into_response())
}

enum UpdateOutcome {
    Updated(Label),
    NotFound,
    Conflict,
}

pub async fn update(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    ApiJson(payload): ApiJson<UpdateLabel>,
) -> Result<Json<Label>, ApiError> {
    require_human(&auth.0)?;
    let name = payload.name.as_deref().map(validate_name).transpose()?;
    let color = payload.color.as_deref().map(validate_color).transpose()?;
    if name.is_none() && color.is_none() {
        return Err(ApiError::validation("no editable fields provided"));
    }

    let outcome = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(label) = label_by_id(&transaction, &id)? else {
                return Ok(UpdateOutcome::NotFound);
            };
            if let Some(ref name) = name {
                let duplicate = transaction
                    .query_row(
                        "SELECT 1 FROM labels WHERE name = ?1 COLLATE NOCASE AND id <> ?2",
                        params![name, label.id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if duplicate {
                    return Ok(UpdateOutcome::Conflict);
                }
                transaction.execute(
                    "UPDATE labels SET name = ?1 WHERE id = ?2",
                    params![name, label.id],
                )?;
            }
            if let Some(ref color) = color {
                transaction.execute(
                    "UPDATE labels SET color = ?1 WHERE id = ?2",
                    params![color, label.id],
                )?;
            }
            let label = label_by_id(&transaction, &label.id)?
                .ok_or_else(|| crate::error::Error::msg("updated label missing"))?;
            transaction.commit()?;
            Ok(UpdateOutcome::Updated(label))
        })
        .await?;

    match outcome {
        UpdateOutcome::Updated(label) => {
            state.publish("label.updated", json!({ "label": &label }));
            Ok(Json(label))
        }
        UpdateOutcome::NotFound => Err(ApiError::not_found("label not found")),
        UpdateOutcome::Conflict => Err(ApiError::conflict("a label with this name already exists")),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    require_human(&auth.0)?;
    let actor = auth.0.actor().to_owned();
    let deleted = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(label) = label_by_id(&transaction, &id)? else {
                return Ok(None);
            };
            let task_ids = {
                let mut statement =
                    transaction.prepare("SELECT task_id FROM task_labels WHERE label_id = ?1")?;
                let rows = statement.query_map([&label.id], |row| row.get::<_, String>(0))?;
                let mut task_ids = Vec::new();
                for row in rows {
                    task_ids.push(row?);
                }
                task_ids
            };
            transaction.execute("DELETE FROM labels WHERE id = ?1", [&label.id])?;
            for task_id in task_ids {
                insert_activity(
                    &transaction,
                    &task_id,
                    &actor,
                    "label_removed",
                    &json!({ "label_id": label.id, "name": label.name }),
                )?;
            }
            transaction.commit()?;
            Ok(Some(label))
        })
        .await?;
    if let Some(label) = deleted {
        state.publish("label.deleted", json!({ "label": &label }));
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(ApiError::not_found("label not found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_names_are_trimmed_and_bounded() {
        assert_eq!(validate_name("  bug  ").expect("name"), "bug");
        assert!(validate_name("").is_err());
        assert!(validate_name(&"x".repeat(33)).is_err());
        assert!(validate_name(&"x".repeat(32)).is_ok());
    }
}
