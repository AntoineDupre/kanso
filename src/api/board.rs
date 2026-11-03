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

    let mut columns = BoardColumns::default();
    for task in &mut tasks {
        task.labels = load_labels(connection, &task.id)?;
        columns.push(BoardCard::from_task(task));
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
