use rusqlite::Row;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Project {
    pub id: String,
    pub key: String,
    pub name: String,
    pub description: String,
    pub repo_path: Option<String>,
    pub color: String,
    pub position: f64,
    pub openchamber_project_id: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub base_branch: Option<String>,
    pub worktree: bool,
    pub prompt_template: Option<String>,
    pub archived_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub const PROJECT_COLUMNS: &str = "id, key, name, description, repo_path, color, position, \
     openchamber_project_id, agent, model, base_branch, worktree, prompt_template, archived_at, \
     created_at, updated_at";

impl Project {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            key: row.get(1)?,
            name: row.get(2)?,
            description: row.get(3)?,
            repo_path: row.get(4)?,
            color: row.get(5)?,
            position: row.get(6)?,
            openchamber_project_id: row.get(7)?,
            agent: row.get(8)?,
            model: row.get(9)?,
            base_branch: row.get(10)?,
            worktree: row.get::<_, i64>(11)? != 0,
            prompt_template: row.get(12)?,
            archived_at: row.get(13)?,
            created_at: row.get(14)?,
            updated_at: row.get(15)?,
        })
    }
}
