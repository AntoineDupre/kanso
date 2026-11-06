use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;

use super::model::{Project, PROJECT_COLUMNS};
use super::{
    deserialize_present, parse_flag, require_human, validate_color, ApiError, ApiJson, ApiQuery,
    Auth,
};
use crate::adapter::openchamber::parse_model;
use crate::ids;
use crate::server::AppState;

const DEFAULT_COLOR: &str = "#64748b";

#[derive(Deserialize)]
pub struct ListQuery {
    archived: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateProject {
    key: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    repo_path: Option<String>,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateProject {
    key: Option<String>,
    name: Option<String>,
    description: Option<String>,
    color: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    repo_path: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    agent: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    model: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    base_branch: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    prompt_template: Option<Option<String>>,
    worktree: Option<bool>,
}

pub fn normalise_key(raw: &str) -> Result<String, ApiError> {
    let key = raw.trim().to_ascii_uppercase();
    if key.chars().count() < 2 || key.chars().count() > 6 {
        return Err(ApiError::validation(
            "project key must be 2 to 6 characters",
        ));
    }
    if !key
        .chars()
        .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
    {
        return Err(ApiError::validation(
            "project key may only contain A-Z and 0-9",
        ));
    }
    Ok(key)
}

fn validate_name(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(ApiError::validation("project name must not be empty"));
    }
    if name.chars().count() > 120 {
        return Err(ApiError::validation(
            "project name must be at most 120 characters",
        ));
    }
    Ok(name.to_owned())
}

/// Trim and validate a project model. An empty value stores `NULL`.
fn validate_model(raw: &str) -> Result<Option<String>, ApiError> {
    let model = raw.trim();
    if model.is_empty() {
        return Ok(None);
    }
    parse_model(model)
        .map_err(|error| ApiError::unprocessable(format!("invalid model: {error}")))?;
    Ok(Some(model.to_owned()))
}

pub(super) fn project_by_ref(
    connection: &rusqlite::Connection,
    reference: &str,
) -> crate::error::Result<Option<Project>> {
    connection
        .query_row(
            &format!(
                "SELECT {PROJECT_COLUMNS} FROM projects
                 WHERE id = ?1 OR key = ?1 COLLATE NOCASE"
            ),
            [reference],
            Project::from_row,
        )
        .optional()
        .map_err(Into::into)
}

pub async fn list(
    State(state): State<AppState>,
    _auth: Auth,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<Project>>, ApiError> {
    let include_archived = parse_flag(query.archived.as_deref(), false)?;
    let projects = state
        .db
        .call(move |connection| {
            let sql = if include_archived {
                format!("SELECT {PROJECT_COLUMNS} FROM projects ORDER BY position, id")
            } else {
                format!(
                    "SELECT {PROJECT_COLUMNS} FROM projects
                     WHERE archived_at IS NULL ORDER BY position, id"
                )
            };
            let mut statement = connection.prepare(&sql)?;
            let projects = statement
                .query_map([], Project::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(projects)
        })
        .await?;
    Ok(Json(projects))
}

pub async fn get(
    State(state): State<AppState>,
    _auth: Auth,
    Path(reference): Path<String>,
) -> Result<Json<Project>, ApiError> {
    let project = state
        .db
        .call(move |connection| project_by_ref(connection, &reference))
        .await?
        .ok_or_else(|| ApiError::not_found("project not found"))?;
    Ok(Json(project))
}

pub async fn create(
    State(state): State<AppState>,
    auth: Auth,
    ApiJson(payload): ApiJson<CreateProject>,
) -> Result<Response, ApiError> {
    require_human(&auth.0)?;
    let key = normalise_key(&payload.key)?;
    let name = validate_name(&payload.name)?;
    let color = match payload.color.as_deref() {
        Some(value) => validate_color(value)?,
        None => DEFAULT_COLOR.to_owned(),
    };
    let description = payload.description.unwrap_or_default();
    let repo_path = payload.repo_path;
    let model = match payload.model.as_deref() {
        Some(raw) => validate_model(raw)?,
        None => None,
    };
    let id = ids::uuid_v4()?;
    let now = super::now_millis();

    let created = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let exists = transaction
                .query_row(
                    "SELECT 1 FROM projects WHERE key = ?1 COLLATE NOCASE",
                    [&key],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if exists {
                return Ok(None);
            }
            let position: f64 = transaction.query_row(
                "SELECT COALESCE(MAX(position), 0) + 1 FROM projects",
                [],
                |row| row.get(0),
            )?;
            transaction.execute(
                "INSERT INTO projects
                 (id, key, name, description, repo_path, color, position, model, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                params![
                    id,
                    key,
                    name,
                    description,
                    repo_path,
                    color,
                    position,
                    model,
                    now
                ],
            )?;
            let project = project_by_ref(&transaction, &id)?
                .ok_or_else(|| crate::error::Error::msg("created project missing"))?;
            transaction.commit()?;
            Ok(Some(project))
        })
        .await?;

    match created {
        Some(project) => {
            state.publish(
                "project.created",
                serde_json::json!({ "project": &project }),
            );
            Ok((StatusCode::CREATED, Json(project)).into_response())
        }
        None => Err(ApiError::conflict("a project with this key already exists")),
    }
}

enum UpdateOutcome {
    Updated(Box<Project>),
    Conflict,
    NotFound,
}

pub async fn update(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
    ApiJson(payload): ApiJson<UpdateProject>,
) -> Result<Json<Project>, ApiError> {
    require_human(&auth.0)?;

    let key = payload.key.as_deref().map(normalise_key).transpose()?;
    let name = payload.name.as_deref().map(validate_name).transpose()?;
    let color = payload.color.as_deref().map(validate_color).transpose()?;
    let model = match payload.model.as_ref() {
        Some(Some(raw)) => validate_model(raw)?,
        Some(None) | None => None,
    };
    let model_present = payload.model.is_some();
    let empty = key.is_none()
        && name.is_none()
        && color.is_none()
        && payload.description.is_none()
        && payload.repo_path.is_none()
        && payload.agent.is_none()
        && !model_present
        && payload.base_branch.is_none()
        && payload.prompt_template.is_none()
        && payload.worktree.is_none();
    if empty {
        return Err(ApiError::validation("no editable fields provided"));
    }

    let now = super::now_millis();
    let outcome = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(mut project) = project_by_ref(&transaction, &reference)? else {
                return Ok(UpdateOutcome::NotFound);
            };
            if let Some(ref key) = key {
                let duplicate = transaction
                    .query_row(
                        "SELECT 1 FROM projects WHERE key = ?1 COLLATE NOCASE AND id <> ?2",
                        params![key, project.id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if duplicate {
                    return Ok(UpdateOutcome::Conflict);
                }
                transaction.execute(
                    "UPDATE projects SET key = ?1 WHERE id = ?2",
                    params![key, project.id],
                )?;
                project.key = key.clone();
            }
            if let Some(ref name) = name {
                transaction.execute(
                    "UPDATE projects SET name = ?1 WHERE id = ?2",
                    params![name, project.id],
                )?;
                project.name = name.clone();
            }
            if let Some(ref description) = payload.description {
                transaction.execute(
                    "UPDATE projects SET description = ?1 WHERE id = ?2",
                    params![description, project.id],
                )?;
                project.description = description.clone();
            }
            if let Some(repo_path) = &payload.repo_path {
                transaction.execute(
                    "UPDATE projects SET repo_path = ?1 WHERE id = ?2",
                    params![repo_path, project.id],
                )?;
                project.repo_path = repo_path.clone();
            }
            if let Some(ref color) = color {
                transaction.execute(
                    "UPDATE projects SET color = ?1 WHERE id = ?2",
                    params![color, project.id],
                )?;
                project.color = color.clone();
            }
            if let Some(agent) = &payload.agent {
                transaction.execute(
                    "UPDATE projects SET agent = ?1 WHERE id = ?2",
                    params![agent, project.id],
                )?;
                project.agent = agent.clone();
            }
            if let Some(model) = &model {
                transaction.execute(
                    "UPDATE projects SET model = ?1 WHERE id = ?2",
                    params![model, project.id],
                )?;
                project.model = Some(model.clone());
            }
            if model_present && model.is_none() {
                transaction.execute(
                    "UPDATE projects SET model = NULL WHERE id = ?1",
                    params![project.id],
                )?;
                project.model = None;
            }
            if let Some(base_branch) = &payload.base_branch {
                transaction.execute(
                    "UPDATE projects SET base_branch = ?1 WHERE id = ?2",
                    params![base_branch, project.id],
                )?;
                project.base_branch = base_branch.clone();
            }
            if let Some(prompt_template) = &payload.prompt_template {
                transaction.execute(
                    "UPDATE projects SET prompt_template = ?1 WHERE id = ?2",
                    params![prompt_template, project.id],
                )?;
                project.prompt_template = prompt_template.clone();
            }
            if let Some(worktree) = payload.worktree {
                transaction.execute(
                    "UPDATE projects SET worktree = ?1 WHERE id = ?2",
                    params![i64::from(worktree), project.id],
                )?;
                project.worktree = worktree;
            }
            transaction.execute(
                "UPDATE projects SET updated_at = ?1 WHERE id = ?2",
                params![now, project.id],
            )?;
            project.updated_at = now;
            transaction.commit()?;
            Ok(UpdateOutcome::Updated(Box::new(project)))
        })
        .await?;

    match outcome {
        UpdateOutcome::Updated(project) => {
            state.publish(
                "project.updated",
                serde_json::json!({ "project": &project }),
            );
            Ok(Json(*project))
        }
        UpdateOutcome::Conflict => {
            Err(ApiError::conflict("a project with this key already exists"))
        }
        UpdateOutcome::NotFound => Err(ApiError::not_found("project not found")),
    }
}

pub async fn archive(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
) -> Result<Json<Project>, ApiError> {
    require_human(&auth.0)?;
    let now = super::now_millis();
    let project = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;
            let Some(project) = project_by_ref(&transaction, &reference)? else {
                return Ok(None);
            };
            transaction.execute(
                "UPDATE projects
                 SET archived_at = COALESCE(archived_at, ?1), updated_at = ?1
                 WHERE id = ?2",
                params![now, project.id],
            )?;
            let project = project_by_ref(&transaction, &project.id)?;
            transaction.commit()?;
            Ok(project)
        })
        .await?
        .ok_or_else(|| ApiError::not_found("project not found"))?;
    state.publish(
        "project.archived",
        serde_json::json!({ "project": &project }),
    );
    Ok(Json(project))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_uppercased_and_validated() {
        assert_eq!(normalise_key("alp").expect("key"), "ALP");
        assert_eq!(normalise_key(" a1 ").expect("key"), "A1");
        assert_eq!(normalise_key("ABCDEF").expect("key"), "ABCDEF");
        assert!(normalise_key("A").is_err());
        assert!(normalise_key("ABCDEFG").is_err());
        assert!(normalise_key("A-B").is_err());
        assert!(normalise_key("A B").is_err());
        assert!(normalise_key("").is_err());
    }

    #[test]
    fn colors_must_be_six_hex_digits() {
        assert_eq!(validate_color("#A1b2C3").expect("color"), "#a1b2c3");
        for invalid in ["#fff", "a1b2c3", "#a1b2c", "#a1b2c3z", "#12345g"] {
            assert!(validate_color(invalid).is_err(), "{invalid}");
        }
        assert_eq!(validate_color(" #123456 ").expect("color"), "#123456");
    }
}
