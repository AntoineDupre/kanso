use std::collections::{HashMap, HashSet};

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;

use super::model::{
    BoardCard, BoardColumns, GlobalBoard, Project, ProjectBoard, PROJECT_COLUMNS, TASK_COLUMNS,
};
use super::projects::project_by_ref;
use super::tasks::load_labels;
use super::{ApiError, ApiQuery, Auth};
use crate::server::AppState;

#[derive(Deserialize)]
pub struct BoardQuery {
    project: Option<String>,
}

fn load_board(
    connection: &rusqlite::Connection,
    project: Project,
) -> crate::error::Result<ProjectBoard> {
    let mut statement = connection.prepare(&format!(
        "SELECT {TASK_COLUMNS} FROM tasks t JOIN projects p ON p.id = t.project_id
         WHERE t.project_id = ?1 AND t.state = 'active'
         ORDER BY t.rank, t.number"
    ))?;
    let mut tasks = statement
        .query_map([&project.id], super::model::Task::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);

    // One checklist aggregate and one blocked set per board, not per card.
    let mut checklist: HashMap<String, (i64, i64)> = HashMap::new();
    {
        let mut statement = connection.prepare(
            "SELECT c.task_id, c.done FROM checklist c
             JOIN tasks t ON t.id = c.task_id
             WHERE t.project_id = ?1",
        )?;
        let rows = statement.query_map([&project.id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (task_id, done) = row?;
            let entry = checklist.entry(task_id).or_insert((0, 0));
            entry.1 += 1;
            if done != 0 {
                entry.0 += 1;
            }
        }
    }

    // Keep this predicate in sync with the `blocked_by` check in
    // `api::tasks::claim_in_transaction`; `blocked` mirrors that dependency
    // gate only, not claimability as a whole.
    let mut blocked: HashSet<String> = HashSet::new();
    {
        let mut statement = connection.prepare(
            "SELECT DISTINCT d.task_id FROM dependencies d
             JOIN tasks dependent ON dependent.id = d.task_id
             JOIN tasks blocker ON blocker.id = d.blocked_by
             WHERE dependent.project_id = ?1 AND blocker.status <> 'done'",
        )?;
        let rows = statement.query_map([&project.id], |row| row.get::<_, String>(0))?;
        for row in rows {
            blocked.insert(row?);
        }
    }

    let mut columns = BoardColumns::default();
    for task in &mut tasks {
        task.labels = load_labels(connection, &task.id)?;
        let (done, total) = checklist.get(&task.id).copied().unwrap_or((0, 0));
        columns.push(BoardCard::from_task(
            task,
            done,
            total,
            blocked.contains(&task.id),
        ));
    }
    Ok(ProjectBoard { project, columns })
}

pub async fn project_board(
    State(state): State<AppState>,
    _auth: Auth,
    Path(reference): Path<String>,
) -> Result<Json<ProjectBoard>, ApiError> {
    let board = state
        .db
        .call(move |connection| {
            let Some(project) = project_by_ref(connection, &reference)? else {
                return Ok(None);
            };
            Ok(Some(load_board(connection, project)?))
        })
        .await?
        .ok_or_else(|| ApiError::not_found("project not found"))?;
    Ok(Json(board))
}

pub async fn global(
    State(state): State<AppState>,
    _auth: Auth,
    ApiQuery(query): ApiQuery<BoardQuery>,
) -> Result<Json<GlobalBoard>, ApiError> {
    let boards = state
        .db
        .call(move |connection| {
            let projects = if let Some(reference) = &query.project {
                let Some(project) = project_by_ref(connection, reference)? else {
                    return Ok(None);
                };
                vec![project]
            } else {
                let mut statement = connection.prepare(&format!(
                    "SELECT {PROJECT_COLUMNS} FROM projects
                     WHERE archived_at IS NULL ORDER BY position, id"
                ))?;
                let projects = statement
                    .query_map([], Project::from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                projects
            };

            let mut boards = Vec::with_capacity(projects.len());
            for project in projects {
                boards.push(load_board(connection, project)?);
            }
            Ok(Some(GlobalBoard { projects: boards }))
        })
        .await?
        .ok_or_else(|| ApiError::not_found("project not found"))?;
    Ok(Json(boards))
}
