use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, OptionalExtension, Row};
use serde::Deserialize;
use serde_json::json;

use super::model::{
    Activity, ChecklistItem, Comment, Dependencies, DependencyRef, Label, Task, TaskDetail,
    TASK_COLUMNS,
};
use super::{
    deserialize_present, now_millis, parse_flag, require_human, ApiError, ApiJson, ApiQuery, Auth,
};
use crate::ids;
use crate::rank;
use crate::server::AppState;

pub const STATUSES: [&str; 6] = [
    "backlog",
    "todo",
    "in_progress",
    "in_review",
    "blocker",
    "done",
];
pub const STATES: [&str; 3] = ["active", "proposed", "archived"];
pub const PRIORITIES: [&str; 3] = ["low", "normal", "high"];

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 500;
const MAX_DESCRIPTION_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
pub struct TaskQuery {
    project: Option<String>,
    status: Option<String>,
    state: Option<String>,
    label: Option<String>,
    q: Option<String>,
    claimed_by: Option<String>,
    attention: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTask {
    project_id: Option<String>,
    project_key: Option<String>,
    title: String,
    #[serde(default)]
    description: Option<String>,
    status: Option<String>,
    priority: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    due_date: Option<Option<String>>,
    labels: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateTask {
    title: Option<String>,
    description: Option<String>,
    priority: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    due_date: Option<Option<String>>,
    labels: Option<Vec<String>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TaskReference {
    Id(String),
    Number { key: String, number: i64 },
}

pub fn parse_task_ref(raw: &str) -> Option<TaskReference> {
    let raw = raw.trim();
    if ids::is_uuid(raw) {
        return Some(TaskReference::Id(raw.to_ascii_lowercase()));
    }
    let (key, number) = raw.rsplit_once('-')?;
    let key = key.to_ascii_uppercase();
    if key.len() < 2
        || key.len() > 6
        || !key
            .chars()
            .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
    {
        return None;
    }
    if number.is_empty() || !number.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    let number: i64 = number.parse().ok()?;
    if number <= 0 {
        return None;
    }
    Some(TaskReference::Number { key, number })
}

pub fn validate_status(value: &str) -> Result<String, ApiError> {
    if STATUSES.contains(&value) {
        Ok(value.to_owned())
    } else {
        Err(ApiError::validation(format!(
            "status must be one of {}",
            STATUSES.join(", ")
        )))
    }
}

pub fn validate_priority(value: &str) -> Result<String, ApiError> {
    if PRIORITIES.contains(&value) {
        Ok(value.to_owned())
    } else {
        Err(ApiError::validation("priority must be low, normal or high"))
    }
}

pub fn validate_title(raw: &str) -> Result<String, ApiError> {
    let title = raw.trim();
    if title.is_empty() {
        return Err(ApiError::validation("title must not be empty"));
    }
    if title.chars().count() > 200 {
        return Err(ApiError::validation("title must be at most 200 characters"));
    }
    Ok(title.to_owned())
}

fn validate_description(raw: &str) -> Result<String, ApiError> {
    if raw.len() > MAX_DESCRIPTION_BYTES {
        return Err(ApiError::validation("description must be at most 64 KiB"));
    }
    Ok(raw.to_owned())
}

pub fn validate_due_date(raw: &str) -> Result<String, ApiError> {
    let parts: Vec<&str> = raw.split('-').collect();
    let valid = parts.len() == 3
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts.iter().all(|part| {
            !part.is_empty() && part.chars().all(|character| character.is_ascii_digit())
        });
    if !valid {
        return Err(ApiError::validation("due_date must be YYYY-MM-DD"));
    }
    let year: i32 = parts[0]
        .parse()
        .map_err(|_| ApiError::validation("invalid year"))?;
    let month: u32 = parts[1]
        .parse()
        .map_err(|_| ApiError::validation("invalid month"))?;
    let day: u32 = parts[2]
        .parse()
        .map_err(|_| ApiError::validation("invalid day"))?;
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    if !(1..=days).contains(&day) {
        return Err(ApiError::validation("due_date is not a real calendar date"));
    }
    Ok(raw.to_owned())
}

pub(super) fn task_by_ref(
    connection: &rusqlite::Connection,
    reference: &str,
) -> crate::error::Result<Option<Task>> {
    let Some(parsed) = parse_task_ref(reference) else {
        return Ok(None);
    };
    let sql = format!("SELECT {TASK_COLUMNS} FROM tasks t JOIN projects p ON p.id = t.project_id");
    match parsed {
        TaskReference::Id(id) => connection
            .query_row(&format!("{sql} WHERE t.id = ?1"), [id], Task::from_row)
            .optional()
            .map_err(Into::into),
        TaskReference::Number { key, number } => connection
            .query_row(
                &format!("{sql} WHERE p.key = ?1 COLLATE NOCASE AND t.number = ?2"),
                params![key, number],
                Task::from_row,
            )
            .optional()
            .map_err(Into::into),
    }
}

pub fn insert_activity(
    connection: &rusqlite::Connection,
    task_id: &str,
    actor: &str,
    kind: &str,
    data: &serde_json::Value,
) -> crate::error::Result<()> {
    connection.execute(
        "INSERT INTO activity (id, task_id, actor, kind, data, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            ids::uuid_v4()?,
            task_id,
            actor,
            kind,
            data.to_string(),
            now_millis()
        ],
    )?;
    Ok(())
}

pub fn load_labels(
    connection: &rusqlite::Connection,
    task_id: &str,
) -> crate::error::Result<Vec<Label>> {
    let mut statement = connection.prepare(
        "SELECT l.id, l.name, l.color FROM task_labels tl
         JOIN labels l ON l.id = tl.label_id
         WHERE tl.task_id = ?1 ORDER BY l.name COLLATE NOCASE, l.id",
    )?;
    let labels = statement
        .query_map([task_id], |row| {
            Ok(Label {
                id: row.get(0)?,
                name: row.get(1)?,
                color: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(labels)
}

fn dependency_ref(row: &Row<'_>) -> rusqlite::Result<DependencyRef> {
    let key: String = row.get(1)?;
    let number: i64 = row.get(2)?;
    Ok(DependencyRef {
        id: row.get(0)?,
        reference: format!("{key}-{number}"),
        title: row.get(3)?,
        status: row.get(4)?,
    })
}

fn load_dependencies(
    connection: &rusqlite::Connection,
    task_id: &str,
) -> crate::error::Result<Dependencies> {
    let mut statement = connection.prepare(
        "SELECT t.id, p.key, t.number, t.title, t.status FROM dependencies d
         JOIN tasks t ON t.id = d.blocked_by
         JOIN projects p ON p.id = t.project_id
         WHERE d.task_id = ?1 ORDER BY p.key, t.number",
    )?;
    let blocked_by = statement
        .query_map([task_id], dependency_ref)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut statement = connection.prepare(
        "SELECT t.id, p.key, t.number, t.title, t.status FROM dependencies d
         JOIN tasks t ON t.id = d.task_id
         JOIN projects p ON p.id = t.project_id
         WHERE d.blocked_by = ?1 ORDER BY p.key, t.number",
    )?;
    let blocks = statement
        .query_map([task_id], dependency_ref)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(Dependencies { blocked_by, blocks })
}

fn load_checklist(
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

fn load_comments(
    connection: &rusqlite::Connection,
    task_id: &str,
) -> crate::error::Result<Vec<Comment>> {
    let mut statement = connection.prepare(
        "SELECT id, author, body, created_at FROM comments
         WHERE task_id = ?1 ORDER BY created_at, id",
    )?;
    let comments = statement
        .query_map([task_id], |row| {
            Ok(Comment {
                id: row.get(0)?,
                author: row.get(1)?,
                body: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(comments)
}

fn load_activity(
    connection: &rusqlite::Connection,
    task_id: &str,
) -> crate::error::Result<Vec<Activity>> {
    let mut statement = connection.prepare(
        "SELECT id, actor, kind, data, created_at FROM activity
         WHERE task_id = ?1 ORDER BY created_at, id",
    )?;
    let activity = statement
        .query_map([task_id], |row| {
            let data: String = row.get(3)?;
            Ok(Activity {
                id: row.get(0)?,
                actor: row.get(1)?,
                kind: row.get(2)?,
                data: serde_json::from_str(&data).unwrap_or_else(|_| json!({})),
                created_at: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(activity)
}

pub fn task_detail(
    connection: &rusqlite::Connection,
    mut task: Task,
) -> crate::error::Result<TaskDetail> {
    task.labels = load_labels(connection, &task.id)?;
    Ok(TaskDetail {
        checklist: load_checklist(connection, &task.id)?,
        comments: load_comments(connection, &task.id)?,
        activity: load_activity(connection, &task.id)?,
        dependencies: load_dependencies(connection, &task.id)?,
        task,
    })
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

pub async fn list(
    State(state): State<AppState>,
    _auth: Auth,
    ApiQuery(query): ApiQuery<TaskQuery>,
) -> Result<Json<Vec<Task>>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let offset = query.offset.unwrap_or(0);
    if let Some(status) = query.status.as_deref() {
        validate_status(status)?;
    }
    if let Some(state_value) = query.state.as_deref() {
        if !STATES.contains(&state_value) {
            return Err(ApiError::validation(format!(
                "state must be one of {}",
                STATES.join(", ")
            )));
        }
    }
    let attention = if query.attention.is_some() {
        Some(parse_flag(query.attention.as_deref(), false)?)
    } else {
        None
    };

    let mut sql = format!(
        "SELECT {TASK_COLUMNS} FROM tasks t JOIN projects p ON p.id = t.project_id WHERE 1 = 1"
    );
    let mut values: Vec<Value> = Vec::new();

    if let Some(project) = query.project {
        sql.push_str(" AND (t.project_id = ? OR p.key = ? COLLATE NOCASE)");
        values.push(Value::Text(project.clone()));
        values.push(Value::Text(project));
    }
    if let Some(status) = query.status {
        sql.push_str(" AND t.status = ?");
        values.push(Value::Text(status));
    }
    if let Some(task_state) = query.state {
        sql.push_str(" AND t.state = ?");
        values.push(Value::Text(task_state));
    }
    if let Some(claimed_by) = query.claimed_by {
        sql.push_str(" AND t.claimed_by = ?");
        values.push(Value::Text(claimed_by));
    }
    if let Some(attention) = attention {
        sql.push_str(" AND t.attention = ?");
        values.push(Value::Integer(i64::from(attention)));
    }
    if let Some(text) = query.q {
        sql.push_str(" AND (t.title LIKE ? ESCAPE '\\' OR t.description LIKE ? ESCAPE '\\')");
        let pattern = format!("%{}%", escape_like(&text));
        values.push(Value::Text(pattern.clone()));
        values.push(Value::Text(pattern));
    }
    if let Some(label) = query.label {
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM task_labels tl JOIN labels l ON l.id = tl.label_id
             WHERE tl.task_id = t.id AND (l.id = ? OR l.name = ? COLLATE NOCASE))",
        );
        values.push(Value::Text(label.clone()));
        values.push(Value::Text(label));
    }
    // Board order, not SQLite's lexical order of the status names.
    sql.push_str(
        " ORDER BY CASE t.status
            WHEN 'backlog' THEN 0 WHEN 'todo' THEN 1 WHEN 'in_progress' THEN 2
            WHEN 'in_review' THEN 3 WHEN 'blocker' THEN 4 WHEN 'done' THEN 5
            ELSE 6 END, t.rank, t.number LIMIT ? OFFSET ?",
    );
    values.push(Value::Integer(i64::from(limit)));
    values.push(Value::Integer(i64::from(offset)));

    let tasks = state
        .db
        .call(move |connection| {
            let mut statement = connection.prepare(&sql)?;
            let mut tasks = statement
                .query_map(params_from_iter(values), Task::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for task in &mut tasks {
                task.labels = load_labels(connection, &task.id)?;
            }
            Ok(tasks)
        })
        .await?;
    Ok(Json(tasks))
}

enum CreateOutcome {
    Created(Box<TaskDetail>),
    UnknownProject,
    UnknownLabel(String),
}

pub async fn create(
    State(state): State<AppState>,
    auth: Auth,
    ApiJson(payload): ApiJson<CreateTask>,
) -> Result<Response, ApiError> {
    require_human(&auth.0)?;
    let title = validate_title(&payload.title)?;
    let description = payload
        .description
        .as_deref()
        .map(validate_description)
        .transpose()?
        .unwrap_or_default();
    let status = match payload.status.as_deref() {
        Some(status) => validate_status(status)?,
        None => "backlog".to_owned(),
    };
    let priority = match payload.priority.as_deref() {
        Some(priority) => validate_priority(priority)?,
        None => "normal".to_owned(),
    };
    let due_date = match payload.due_date {
        Some(Some(value)) => Some(validate_due_date(&value)?),
        Some(None) | None => None,
    };
    let labels = payload.labels.unwrap_or_default();
    let project_id = payload.project_id;
    let project_key = payload.project_key;
    if project_id.is_none() && project_key.is_none() {
        return Err(ApiError::validation(
            "project_id or project_key is required",
        ));
    }
    // When both are sent, the id wins; the key is a convenience fallback.

    let actor = auth.0.actor().to_owned();
    let id = ids::uuid_v4()?;
    let now = now_millis();
    let outcome = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let project = if let Some(project_id) = &project_id {
                transaction
                    .query_row(
                        "SELECT id FROM projects WHERE id = ?1",
                        [project_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
            } else if let Some(project_key) = &project_key {
                transaction
                    .query_row(
                        "SELECT id FROM projects WHERE key = ?1 COLLATE NOCASE",
                        [project_key],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
            } else {
                None
            };
            let Some(project_id) = project else {
                return Ok(CreateOutcome::UnknownProject);
            };

            let mut label_ids = Vec::new();
            for value in &labels {
                let found = transaction
                    .query_row(
                        "SELECT id FROM labels WHERE id = ?1 OR name = ?1 COLLATE NOCASE",
                        [value],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                match found {
                    Some(label_id) => {
                        if !label_ids.contains(&label_id) {
                            label_ids.push(label_id);
                        }
                    }
                    None => return Ok(CreateOutcome::UnknownLabel(value.clone())),
                }
            }

            let number: i64 = transaction.query_row(
                "SELECT COALESCE(MAX(number), 0) + 1 FROM tasks WHERE project_id = ?1",
                [&project_id],
                |row| row.get(0),
            )?;
            let max_rank: Option<f64> = transaction.query_row(
                "SELECT MAX(rank) FROM tasks WHERE project_id = ?1 AND status = ?2",
                params![project_id, status],
                |row| row.get(0),
            )?;
            let rank = rank::append(max_rank);
            let done_at = (status == "done").then_some(now);
            transaction.execute(
                "INSERT INTO tasks
                 (id, project_id, number, title, description, status, state, rank, priority,
                  due_date, source, created_by, done_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, ?9, 'human', ?10, ?11, ?12, ?12)",
                params![
                    id,
                    project_id,
                    number,
                    title,
                    description,
                    status,
                    rank,
                    priority,
                    due_date,
                    actor,
                    done_at,
                    now
                ],
            )?;
            for label_id in &label_ids {
                transaction.execute(
                    "INSERT INTO task_labels (task_id, label_id) VALUES (?1, ?2)",
                    params![id, label_id],
                )?;
            }
            insert_activity(
                &transaction,
                &id,
                &actor,
                "created",
                &json!({ "number": number, "status": status }),
            )?;
            for label_id in &label_ids {
                insert_activity(
                    &transaction,
                    &id,
                    &actor,
                    "label_added",
                    &json!({ "label_id": label_id }),
                )?;
            }

            let task = task_by_ref(&transaction, &id)?
                .ok_or_else(|| crate::error::Error::msg("created task missing"))?;
            let detail = task_detail(&transaction, task)?;
            transaction.commit()?;
            Ok(CreateOutcome::Created(Box::new(detail)))
        })
        .await?;

    match outcome {
        CreateOutcome::Created(detail) => {
            state.publish("task.created", json!({ "task": &detail.task }));
            Ok((StatusCode::CREATED, Json(*detail)).into_response())
        }
        CreateOutcome::UnknownProject => Err(ApiError::not_found("project not found")),
        CreateOutcome::UnknownLabel(label) => {
            Err(ApiError::validation(format!("unknown label {label:?}")))
        }
    }
}

pub async fn get(
    State(state): State<AppState>,
    _auth: Auth,
    Path(reference): Path<String>,
) -> Result<Json<TaskDetail>, ApiError> {
    let detail = state
        .db
        .call(move |connection| {
            let Some(task) = task_by_ref(connection, &reference)? else {
                return Ok(None);
            };
            Ok(Some(task_detail(connection, task)?))
        })
        .await?
        .ok_or_else(|| ApiError::not_found("task not found"))?;
    Ok(Json(detail))
}

enum UpdateOutcome {
    Updated(Box<TaskDetail>),
    NotFound,
    UnknownLabel(String),
}

pub async fn update(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
    ApiJson(payload): ApiJson<UpdateTask>,
) -> Result<Json<TaskDetail>, ApiError> {
    require_human(&auth.0)?;
    let title = payload.title.as_deref().map(validate_title).transpose()?;
    let description = payload
        .description
        .as_deref()
        .map(validate_description)
        .transpose()?;
    let priority = payload
        .priority
        .as_deref()
        .map(validate_priority)
        .transpose()?;
    let due_date = match payload.due_date {
        Some(Some(value)) => Some(Some(validate_due_date(&value)?)),
        Some(None) => Some(None),
        None => None,
    };
    if title.is_none()
        && description.is_none()
        && priority.is_none()
        && due_date.is_none()
        && payload.labels.is_none()
    {
        return Err(ApiError::validation("no editable fields provided"));
    }

    let actor = auth.0.actor().to_owned();
    let now = now_millis();
    let labels = payload.labels;
    let outcome = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(task) = task_by_ref(&transaction, &reference)? else {
                return Ok(UpdateOutcome::NotFound);
            };
            let mut fields = Vec::new();

            if let Some(ref title) = title {
                transaction.execute(
                    "UPDATE tasks SET title = ?1 WHERE id = ?2",
                    params![title, task.id],
                )?;
                fields.push("title");
            }
            if let Some(ref description) = description {
                transaction.execute(
                    "UPDATE tasks SET description = ?1 WHERE id = ?2",
                    params![description, task.id],
                )?;
                fields.push("description");
            }
            if let Some(ref priority) = priority {
                transaction.execute(
                    "UPDATE tasks SET priority = ?1 WHERE id = ?2",
                    params![priority, task.id],
                )?;
                fields.push("priority");
            }
            if let Some(due_date) = &due_date {
                transaction.execute(
                    "UPDATE tasks SET due_date = ?1 WHERE id = ?2",
                    params![due_date, task.id],
                )?;
                fields.push("due_date");
            }

            if let Some(ref labels) = labels {
                let mut desired: Vec<(String, String)> = Vec::new();
                for value in labels {
                    let found = transaction
                        .query_row(
                            "SELECT id, name FROM labels
                             WHERE id = ?1 OR name = ?1 COLLATE NOCASE",
                            [value],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                        )
                        .optional()?;
                    match found {
                        Some(pair) => {
                            if !desired.iter().any(|(id, _)| id == &pair.0) {
                                desired.push(pair);
                            }
                        }
                        None => return Ok(UpdateOutcome::UnknownLabel(value.clone())),
                    }
                }

                let mut current = Vec::new();
                let mut statement =
                    transaction.prepare("SELECT label_id FROM task_labels WHERE task_id = ?1")?;
                let rows = statement.query_map([&task.id], |row| row.get::<_, String>(0))?;
                for row in rows {
                    current.push(row?);
                }
                drop(statement);

                for label_id in &current {
                    if !desired.iter().any(|(id, _)| id == label_id) {
                        transaction.execute(
                            "DELETE FROM task_labels WHERE task_id = ?1 AND label_id = ?2",
                            params![task.id, label_id],
                        )?;
                        insert_activity(
                            &transaction,
                            &task.id,
                            &actor,
                            "label_removed",
                            &json!({ "label_id": label_id }),
                        )?;
                    }
                }
                for (label_id, _name) in &desired {
                    if !current.contains(label_id) {
                        transaction.execute(
                            "INSERT INTO task_labels (task_id, label_id) VALUES (?1, ?2)",
                            params![task.id, label_id],
                        )?;
                        insert_activity(
                            &transaction,
                            &task.id,
                            &actor,
                            "label_added",
                            &json!({ "label_id": label_id }),
                        )?;
                    }
                }
                fields.push("labels");
            }

            transaction.execute(
                "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
                params![now, task.id],
            )?;
            insert_activity(
                &transaction,
                &task.id,
                &actor,
                "updated",
                &json!({ "fields": fields }),
            )?;

            let task = task_by_ref(&transaction, &task.id)?
                .ok_or_else(|| crate::error::Error::msg("updated task missing"))?;
            let detail = task_detail(&transaction, task)?;
            transaction.commit()?;
            Ok(UpdateOutcome::Updated(Box::new(detail)))
        })
        .await?;

    match outcome {
        UpdateOutcome::Updated(detail) => {
            state.publish("task.updated", json!({ "task": &detail.task }));
            Ok(Json(*detail))
        }
        UpdateOutcome::NotFound => Err(ApiError::not_found("task not found")),
        UpdateOutcome::UnknownLabel(label) => {
            Err(ApiError::validation(format!("unknown label {label:?}")))
        }
    }
}

pub async fn archive(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
) -> Result<Json<TaskDetail>, ApiError> {
    set_archived(state, auth, reference, true).await
}

pub async fn restore(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
) -> Result<Json<TaskDetail>, ApiError> {
    set_archived(state, auth, reference, false).await
}

async fn set_archived(
    state: AppState,
    auth: Auth,
    reference: String,
    archived: bool,
) -> Result<Json<TaskDetail>, ApiError> {
    require_human(&auth.0)?;
    let actor = auth.0.actor().to_owned();
    let now = now_millis();
    let detail = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(task) = task_by_ref(&transaction, &reference)? else {
                return Ok(None);
            };
            let already = if archived {
                task.state == "archived"
            } else {
                task.state != "archived"
            };
            if !already {
                if archived {
                    transaction.execute(
                        "UPDATE tasks
                         SET state = 'archived', archived_at = ?1, updated_at = ?1
                         WHERE id = ?2",
                        params![now, task.id],
                    )?;
                    insert_activity(&transaction, &task.id, &actor, "archived", &json!({}))?;
                } else {
                    transaction.execute(
                        "UPDATE tasks
                         SET state = 'active', archived_at = NULL, updated_at = ?1
                         WHERE id = ?2",
                        params![now, task.id],
                    )?;
                    insert_activity(&transaction, &task.id, &actor, "restored", &json!({}))?;
                }
            }
            let task = task_by_ref(&transaction, &task.id)?
                .ok_or_else(|| crate::error::Error::msg("task missing"))?;
            let detail = task_detail(&transaction, task)?;
            transaction.commit()?;
            Ok(Some(detail))
        })
        .await?
        .ok_or_else(|| ApiError::not_found("task not found"))?;
    let event = if archived {
        "task.archived"
    } else {
        "task.restored"
    };
    state.publish(event, json!({ "task": &detail.task }));
    Ok(Json(detail))
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    hard: Option<String>,
}

pub async fn delete(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
    ApiQuery(query): ApiQuery<DeleteQuery>,
) -> Result<Response, ApiError> {
    require_human(&auth.0)?;
    if !parse_flag(query.hard.as_deref(), false)? {
        return Err(ApiError::validation("hard delete requires ?hard=true"));
    }
    let deleted = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(task) = task_by_ref(&transaction, &reference)? else {
                return Ok(None);
            };
            transaction.execute("DELETE FROM tasks WHERE id = ?1", [&task.id])?;
            transaction.commit()?;
            Ok(Some(task))
        })
        .await?;
    if let Some(task) = deleted {
        state.publish("task.deleted", json!({ "task": &task }));
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(ApiError::not_found("task not found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_parse_keys_and_uuids_case_insensitively() {
        assert_eq!(
            parse_task_ref("ALP-42"),
            Some(TaskReference::Number {
                key: "ALP".to_owned(),
                number: 42
            })
        );
        assert_eq!(
            parse_task_ref("alp-1"),
            Some(TaskReference::Number {
                key: "ALP".to_owned(),
                number: 1
            })
        );
        assert_eq!(
            parse_task_ref("A1-7"),
            Some(TaskReference::Number {
                key: "A1".to_owned(),
                number: 7
            })
        );
        let uuid = "123e4567-e89b-42d3-a456-426614174000";
        assert_eq!(
            parse_task_ref(uuid),
            Some(TaskReference::Id(uuid.to_owned()))
        );
        assert_eq!(
            parse_task_ref(&uuid.to_ascii_uppercase()),
            Some(TaskReference::Id(uuid.to_owned()))
        );

        for invalid in [
            "",
            "ALP",
            "ALP-0",
            "ALP--1",
            "TOOLONG-1",
            "ALP-x",
            "-1",
            "ALP-1-2",
        ] {
            assert_eq!(parse_task_ref(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn statuses_priorities_and_dates_are_validated() {
        assert_eq!(validate_status("todo").expect("status"), "todo");
        assert!(validate_status("doing").is_err());
        assert_eq!(validate_priority("high").expect("priority"), "high");
        assert!(validate_priority("urgent").is_err());

        assert_eq!(validate_due_date("2026-11-12").expect("date"), "2026-11-12");
        assert_eq!(validate_due_date("2024-02-29").expect("leap"), "2024-02-29");
        for invalid in ["2025-02-29", "2026-13-01", "2026-04-31", "2026-1-01", "x"] {
            assert!(validate_due_date(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn titles_and_descriptions_respect_limits() {
        assert_eq!(validate_title("  hello  ").expect("title"), "hello");
        assert!(validate_title("   ").is_err());
        assert!(validate_title(&"x".repeat(201)).is_err());
        assert!(validate_description(&"x".repeat(MAX_DESCRIPTION_BYTES + 1)).is_err());
        assert!(validate_description(&"x".repeat(MAX_DESCRIPTION_BYTES)).is_ok());
    }
}
