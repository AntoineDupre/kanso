use std::collections::HashSet;

use axum::{
    extract::{Path, State},
    Json,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{projects::project_by_ref, require_human, ApiError, ApiJson, Auth};
use crate::importer::gitlab::{normalize_instance, valid_repository};
use crate::server::AppState;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub instance: String,
    pub repository: String,
    pub issues: bool,
    pub review: bool,
    pub follow: bool,
}

fn rows(db: &rusqlite::Connection, project: &str) -> crate::error::Result<Vec<Repository>> {
    let mut statement = db.prepare(
        "SELECT instance, repository, issues, review, follow FROM gitlab_repositories
         WHERE project_id=?1 ORDER BY instance, repository COLLATE NOCASE",
    )?;
    let result = statement
        .query_map([project], |row| {
            Ok(Repository {
                instance: row.get(0)?,
                repository: row.get(1)?,
                issues: row.get(2)?,
                review: row.get(3)?,
                follow: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(result)
}

pub async fn list(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
) -> Result<Json<Vec<Repository>>, ApiError> {
    require_human(&auth.0)?;
    let result = state
        .db
        .call(move |db| {
            let Some(project) = project_by_ref(db, &reference)? else {
                return Ok(None);
            };
            Ok(Some(rows(db, &project.id)?))
        })
        .await?;
    Ok(Json(
        result.ok_or_else(|| ApiError::not_found("project not found"))?,
    ))
}

pub async fn replace(
    State(state): State<AppState>,
    auth: Auth,
    Path(reference): Path<String>,
    ApiJson(mut repositories): ApiJson<Vec<Repository>>,
) -> Result<Json<Vec<Repository>>, ApiError> {
    require_human(&auth.0)?;
    if repositories.len() > 50 {
        return Err(ApiError::validation(
            "At most 50 GitLab repositories are allowed",
        ));
    }
    let mut seen = HashSet::new();
    for repository in &mut repositories {
        repository.instance = normalize_instance(&repository.instance)
            .ok_or_else(|| ApiError::validation("GitLab instance must be an HTTPS origin without credentials, path, query or fragment"))?;
        if !valid_repository(&repository.repository)
            || !seen.insert((
                repository.instance.clone(),
                repository.repository.to_ascii_lowercase(),
            ))
        {
            return Err(ApiError::validation(
                "Invalid or duplicate GitLab repository",
            ));
        }
    }
    let result = state.db.call(move |db| {
        let tx = db.transaction()?;
        let Some(project) = project_by_ref(&tx, &reference)? else { return Ok(None) };
        if project.archived_at.is_some() { return Ok(None); }
        tx.execute("DELETE FROM gitlab_repositories WHERE project_id=?1", [&project.id])?;
        for repository in repositories {
            tx.execute(
                "INSERT INTO gitlab_repositories (project_id, instance, repository, issues, review, follow)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![project.id, repository.instance, repository.repository, repository.issues, repository.review, repository.follow],
            )?;
        }
        let result = rows(&tx, &project.id)?;
        tx.commit()?;
        Ok(Some(result))
    }).await?;
    Ok(Json(result.ok_or_else(|| {
        ApiError::not_found("active project not found")
    })?))
}
