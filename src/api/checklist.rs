use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::json;

use super::model::ChecklistItem;
use super::tasks::{insert_activity, task_by_ref};
use super::{require_human, ApiError, ApiJson, Auth};
use crate::ids;
use crate::rank;
use crate::server::AppState;

const MAX_TEXT_CHARS: usize = 500;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateChecklistItem {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateChecklistItem {
    text: Option<String>,
    done: Option<bool>,
    before: Option<String>,
    after: Option<String>,
}

fn validate_text(raw: &str) -> Result<String, ApiError> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(ApiError::validation("checklist text must not be empty"));
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(ApiError::validation(
            "checklist text must be at most 500 characters",
        ));
    }
    Ok(text.to_owned())
}

fn item_by_id(
    connection: &rusqlite::Connection,
    task_id: &str,
    item_id: &str,
) -> crate::error::Result<Option<ChecklistItem>> {
    connection
        .query_row(
            "SELECT id, text, done, position FROM checklist
             WHERE id = ?1 AND task_id = ?2",
            params![item_id, task_id],
            |row| {
                Ok(ChecklistItem {
                    id: row.get(0)?,
                    text: row.get(1)?,
                    done: row.get::<_, i64>(2)? != 0,
                    position: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn load_items(
    connection: &rusqlite::Connection,
    task_id: &str,
) -> crate::error::Result<Vec<ChecklistItem>> {
    let mut statement = connection.prepare(
        "SELECT id, text, done, position FROM checklist
         WHERE task_id = ?1 ORDER BY position, id",
    )?;
    let items = statement
        .query_map([task_id], |row| {
            Ok(ChecklistItem {
                id: row.get(0)?,
                text: row.get(1)?,
                done: row.get::<_, i64>(2)? != 0,
                position: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(items)
}

fn renumber(connection: &rusqlite::Connection, task_id: &str) -> crate::error::Result<()> {
    let ids = {
        let mut statement = connection
            .prepare("SELECT id FROM checklist WHERE task_id = ?1 ORDER BY position, id")?;
        let rows = statement.query_map([task_id], |row| row.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        ids
    };
    for (index, id) in ids.iter().enumerate() {
        connection.execute(
            "UPDATE checklist SET position = ?1 WHERE id = ?2",
            params![(index + 1) as f64, id],
        )?;
    }
    Ok(())
}

fn ordered_except(
    connection: &rusqlite::Connection,
    task_id: &str,
    item_id: &str,
) -> crate::error::Result<Vec<(String, f64)>> {
    let mut statement = connection.prepare(
        "SELECT id, position FROM checklist
         WHERE task_id = ?1 AND id <> ?2 ORDER BY position, id",
    )?;
    let items = statement
        .query_map(params![task_id, item_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(items)
}

#[derive(Debug)]
enum RankError {
    Missing(String),
    InvalidOrder,
}

fn intended_rank(
    items: &[(String, f64)],
    before: Option<&str>,
    after: Option<&str>,
) -> Result<Option<f64>, RankError> {
    let find = |id: &str| {
        items
            .iter()
            .position(|(item_id, _)| item_id == id)
            .ok_or_else(|| RankError::Missing(id.to_owned()))
    };

    let lower = match after {
        Some(id) => Some(items[find(id)?].1),
        None => match before {
            Some(id) => {
                let index = find(id)?;
                (index > 0).then(|| items[index - 1].1)
            }
            None => None,
        },
    };
    let upper = match before {
        Some(id) => Some(items[find(id)?].1),
        None => match after {
            Some(id) => {
                let index = find(id)?;
                items.get(index + 1).map(|(_, position)| *position)
            }
            None => None,
        },
    };

    if let (Some(lower), Some(upper)) = (lower, upper) {
        if lower >= upper {
            return Err(RankError::InvalidOrder);
        }
    }

    Ok(match (lower, upper) {
        (Some(lower), Some(upper)) => rank::between(lower, upper),
        (Some(lower), None) => Some(lower + 1.0),
        (None, Some(upper)) => Some(upper - 1.0),
        (None, None) => Some(1.0),
    })
}

pub async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
    ApiJson(payload): ApiJson<CreateChecklistItem>,
) -> Result<Response, ApiError> {
    let text = validate_text(&payload.text)?;
    let actor = auth.0.actor().to_owned();
    let id = ids::uuid_v4()?;

    let created = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(task) = task_by_ref(&transaction, &reference)? else {
                return Ok(None);
            };
            let max_position: Option<f64> = transaction.query_row(
                "SELECT MAX(position) FROM checklist WHERE task_id = ?1",
                [&task.id],
                |row| row.get(0),
            )?;
            let position = rank::append(max_position);
            transaction.execute(
                "INSERT INTO checklist (id, task_id, text, done, position)
                 VALUES (?1, ?2, ?3, 0, ?4)",
                params![id, task.id, text, position],
            )?;
            insert_activity(
                &transaction,
                &task.id,
                &actor,
                "checklist_added",
                &json!({ "item_id": id, "text": text }),
            )?;
            transaction.commit()?;
            Ok(Some((
                task.id,
                ChecklistItem {
                    id,
                    text,
                    done: false,
                    position,
                },
            )))
        })
        .await?
        .ok_or_else(|| ApiError::not_found("task not found"))?;
    let (task_id, item) = created;
    state.publish(
        "checklist.created",
        json!({ "task_id": task_id, "item": &item }),
    );
    Ok((StatusCode::CREATED, Json(item)).into_response())
}

enum UpdateOutcome {
    Updated(String, ChecklistItem, Vec<ChecklistItem>),
    NotFound,
    MissingNeighbour(String),
    InvalidOrder,
}

pub async fn update(
    State(state): State<AppState>,
    auth: Auth,
    Path((reference, item_id)): Path<(String, String)>,
    ApiJson(payload): ApiJson<UpdateChecklistItem>,
) -> Result<Json<ChecklistItem>, ApiError> {
    let text = payload.text.as_deref().map(validate_text).transpose()?;
    if text.is_none()
        && payload.done.is_none()
        && payload.before.is_none()
        && payload.after.is_none()
    {
        return Err(ApiError::validation("no editable fields provided"));
    }
    let actor = auth.0.actor().to_owned();
    let before = payload.before;
    let after = payload.after;
    let done = payload.done;

    let outcome = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(task) = task_by_ref(&transaction, &reference)? else {
                return Ok(UpdateOutcome::NotFound);
            };
            let Some(item) = item_by_id(&transaction, &task.id, &item_id)? else {
                return Ok(UpdateOutcome::NotFound);
            };

            let mut fields = Vec::new();
            if let Some(ref text) = text {
                transaction.execute(
                    "UPDATE checklist SET text = ?1 WHERE id = ?2",
                    params![text, item.id],
                )?;
                fields.push("text");
            }
            if let Some(done) = done {
                transaction.execute(
                    "UPDATE checklist SET done = ?1 WHERE id = ?2",
                    params![i64::from(done), item.id],
                )?;
                fields.push("done");
            }

            if before.is_some() || after.is_some() {
                let items = ordered_except(&transaction, &task.id, &item.id)?;
                let computed = match intended_rank(&items, before.as_deref(), after.as_deref()) {
                    Ok(rank) => rank,
                    Err(RankError::Missing(id)) => return Ok(UpdateOutcome::MissingNeighbour(id)),
                    Err(RankError::InvalidOrder) => return Ok(UpdateOutcome::InvalidOrder),
                };
                let computed = match computed {
                    Some(rank) => Some(rank),
                    None => {
                        renumber(&transaction, &task.id)?;
                        let items = ordered_except(&transaction, &task.id, &item.id)?;
                        match intended_rank(&items, before.as_deref(), after.as_deref()) {
                            Ok(rank) => rank,
                            Err(RankError::Missing(id)) => {
                                return Ok(UpdateOutcome::MissingNeighbour(id))
                            }
                            Err(RankError::InvalidOrder) => return Ok(UpdateOutcome::InvalidOrder),
                        }
                    }
                };
                if let Some(position_value) = computed {
                    transaction.execute(
                        "UPDATE checklist SET position = ?1 WHERE id = ?2",
                        params![position_value, item.id],
                    )?;
                    fields.push("position");
                }
            }

            insert_activity(
                &transaction,
                &task.id,
                &actor,
                "checklist_updated",
                &json!({ "item_id": item.id, "fields": fields }),
            )?;
            let item = item_by_id(&transaction, &task.id, &item.id)?
                .ok_or_else(|| crate::error::Error::msg("updated checklist item missing"))?;
            // Include the whole ordered list: a renumber rewrites siblings too.
            let checklist = load_items(&transaction, &task.id)?;
            transaction.commit()?;
            Ok(UpdateOutcome::Updated(task.id, item, checklist))
        })
        .await?;

    match outcome {
        UpdateOutcome::Updated(task_id, item, checklist) => {
            state.publish(
                "checklist.updated",
                json!({ "task_id": task_id, "item": &item, "checklist": checklist }),
            );
            Ok(Json(item))
        }
        UpdateOutcome::NotFound => Err(ApiError::not_found("checklist item not found")),
        UpdateOutcome::MissingNeighbour(id) => {
            Err(ApiError::validation(format!("neighbour {id:?} not found")))
        }
        UpdateOutcome::InvalidOrder => Err(ApiError::validation(
            "before must come after after in the current order",
        )),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    auth: Auth,
    Path((reference, item_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    require_human(&auth.0)?;
    let actor = auth.0.actor().to_owned();
    let deleted = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(task) = task_by_ref(&transaction, &reference)? else {
                return Ok(None);
            };
            let Some(item) = item_by_id(&transaction, &task.id, &item_id)? else {
                return Ok(None);
            };
            transaction.execute("DELETE FROM checklist WHERE id = ?1", [&item.id])?;
            insert_activity(
                &transaction,
                &task.id,
                &actor,
                "checklist_removed",
                &json!({ "item_id": item.id, "text": item.text }),
            )?;
            transaction.commit()?;
            Ok(Some((task.id, item.id)))
        })
        .await?;
    if let Some((task_id, item_id)) = deleted {
        state.publish(
            "checklist.deleted",
            json!({ "task_id": task_id, "item_id": item_id }),
        );
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(ApiError::not_found("checklist item not found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intended_rank_understands_before_and_after() {
        let items = vec![
            ("a".to_owned(), 1.0),
            ("b".to_owned(), 2.0),
            ("c".to_owned(), 3.0),
        ];
        assert_eq!(
            intended_rank(&items, Some("c"), None).expect("before"),
            Some(2.5)
        );
        assert_eq!(
            intended_rank(&items, None, Some("a")).expect("after"),
            Some(1.5)
        );
        assert_eq!(
            intended_rank(&items, None, Some("c")).expect("after"),
            Some(4.0)
        );
        assert_eq!(
            intended_rank(&items, Some("c"), Some("a")).expect("between"),
            Some(2.0)
        );
        assert!(matches!(
            intended_rank(&items, Some("a"), Some("c")),
            Err(RankError::InvalidOrder)
        ));
        assert!(matches!(
            intended_rank(&items, Some("missing"), None),
            Err(RankError::Missing(_))
        ));
    }

    #[test]
    fn checklist_text_is_trimmed_and_bounded() {
        assert_eq!(validate_text("  step  ").expect("text"), "step");
        assert!(validate_text("  ").is_err());
        assert!(validate_text(&"x".repeat(501)).is_err());
        assert!(validate_text(&"x".repeat(500)).is_ok());
    }
}
