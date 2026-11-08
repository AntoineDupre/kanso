use std::collections::HashSet;

use axum::{
    extract::{Path, State},
    Json,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{projects::project_by_ref, require_human, ApiError, ApiJson, Auth};
use crate::importer::github::valid_repository;
use crate::server::AppState;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub repository: String,
    pub issues: bool,
    pub review: bool,
    pub follow: bool,
}

fn rows(db: &rusqlite::Connection, project: &str) -> crate::error::Result<Vec<Repository>> {
    let mut stmt = db.prepare("SELECT repository, issues, review, follow FROM github_repositories WHERE project_id=?1 ORDER BY repository COLLATE NOCASE")?;
    let result = stmt
        .query_map([project], |row| {
            Ok(Repository {
                repository: row.get(0)?,
                issues: row.get(1)?,
                review: row.get(2)?,
                follow: row.get(3)?,
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
    ApiJson(repositories): ApiJson<Vec<Repository>>,
) -> Result<Json<Vec<Repository>>, ApiError> {
    require_human(&auth.0)?;
    let mut seen = HashSet::new();
    if repositories.len() > 50
        || repositories.iter().any(|r| {
            !valid_repository(&r.repository) || !seen.insert(r.repository.to_ascii_lowercase())
        })
    {
        return Err(ApiError::validation(
            "Invalid or duplicate GitHub repository",
        ));
    }
    let result = state.db.call(move |db| {
        let tx = db.transaction()?;
        let Some(project) = project_by_ref(&tx, &reference)? else { return Ok(None) };
        if project.archived_at.is_some() { return Ok(None); }
        tx.execute("DELETE FROM github_repositories WHERE project_id=?1", [&project.id])?;
        for repo in repositories {
            tx.execute("INSERT INTO github_repositories (project_id, repository, issues, review, follow) VALUES (?1,?2,?3,?4,?5)", params![project.id, repo.repository, repo.issues, repo.review, repo.follow])?;
        }
        let result = rows(&tx, &project.id)?;
        tx.commit()?;
        Ok(Some(result))
    }).await?;
    Ok(Json(result.ok_or_else(|| {
        ApiError::not_found("active project not found")
    })?))
}
