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

#[derive(Clone, Debug, Serialize)]
pub struct Label {
    pub id: String,
    pub name: String,
    pub color: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChecklistItem {
    pub id: String,
    pub text: String,
    pub done: bool,
    pub position: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub body: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Activity {
    pub id: String,
    pub actor: String,
    pub kind: String,
    pub data: serde_json::Value,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct DependencyRef {
    pub id: String,
    pub reference: String,
    pub title: String,
    pub status: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Dependencies {
    pub blocked_by: Vec<DependencyRef>,
    pub blocks: Vec<DependencyRef>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Task {
    pub id: String,
    pub project_id: String,
    pub project_key: String,
    pub number: i64,
    pub reference: String,
    pub title: String,
    pub description: String,
    pub status: String,
    pub state: String,
    pub blocked_from: Option<String>,
    pub blocked_reason: Option<String>,
    pub rank: f64,
    pub priority: String,
    pub due_date: Option<String>,
    pub source: String,
    pub created_by: Option<String>,
    pub claimed_by: Option<String>,
    pub session_id: Option<String>,
    pub session_directory: Option<String>,
    pub session_status: Option<String>,
    pub attention: bool,
    pub done_at: Option<i64>,
    pub archived_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub labels: Vec<Label>,
}

pub const TASK_COLUMNS: &str = "t.id, t.project_id, p.key, t.number, t.title, t.description, \
     t.status, t.state, t.blocked_from, t.blocked_reason, t.rank, t.priority, t.due_date, \
     t.source, t.created_by, t.claimed_by, t.session_id, t.session_directory, t.session_status, \
     t.attention, t.done_at, t.archived_at, t.created_at, t.updated_at";

impl Task {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let project_key: String = row.get(2)?;
        let number: i64 = row.get(3)?;
        Ok(Self {
            id: row.get(0)?,
            project_id: row.get(1)?,
            project_key: project_key.clone(),
            number,
            reference: format!("{project_key}-{number}"),
            title: row.get(4)?,
            description: row.get(5)?,
            status: row.get(6)?,
            state: row.get(7)?,
            blocked_from: row.get(8)?,
            blocked_reason: row.get(9)?,
            rank: row.get(10)?,
            priority: row.get(11)?,
            due_date: row.get(12)?,
            source: row.get(13)?,
            created_by: row.get(14)?,
            claimed_by: row.get(15)?,
            session_id: row.get(16)?,
            session_directory: row.get(17)?,
            session_status: row.get(18)?,
            attention: row.get::<_, i64>(19)? != 0,
            done_at: row.get(20)?,
            archived_at: row.get(21)?,
            created_at: row.get(22)?,
            updated_at: row.get(23)?,
            labels: Vec::new(),
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskDetail {
    #[serde(flatten)]
    pub task: Task,
    pub checklist: Vec<ChecklistItem>,
    pub comments: Vec<Comment>,
    pub activity: Vec<Activity>,
    pub dependencies: Dependencies,
}

/// The small card shape sent by the board endpoints (not the full detail).
#[derive(Clone, Debug, Serialize)]
pub struct BoardCard {
    pub id: String,
    pub reference: String,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub due_date: Option<String>,
    pub labels: Vec<Label>,
    pub claimed_by: Option<String>,
    pub attention: bool,
    pub session_status: Option<String>,
    pub blocked_from: Option<String>,
    /// Checklist progress so cards can show `2/3` without opening each card.
    pub checklist_done: i64,
    pub checklist_total: i64,
    /// At least one `blocked_by` card is not done; shown as a lock on the card.
    pub blocked: bool,
}

impl BoardCard {
    pub fn from_task(
        task: &Task,
        checklist_done: i64,
        checklist_total: i64,
        blocked: bool,
    ) -> Self {
        Self {
            id: task.id.clone(),
            reference: task.reference.clone(),
            title: task.title.clone(),
            status: task.status.clone(),
            priority: task.priority.clone(),
            due_date: task.due_date.clone(),
            labels: task.labels.clone(),
            claimed_by: task.claimed_by.clone(),
            attention: task.attention,
            session_status: task.session_status.clone(),
            blocked_from: task.blocked_from.clone(),
            checklist_done,
            checklist_total,
            blocked,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BoardColumns {
    pub backlog: Vec<BoardCard>,
    pub todo: Vec<BoardCard>,
    pub in_progress: Vec<BoardCard>,
    pub in_review: Vec<BoardCard>,
    pub blocker: Vec<BoardCard>,
    pub done: Vec<BoardCard>,
}

impl BoardColumns {
    pub fn push(&mut self, card: BoardCard) {
        match card.status.as_str() {
            "backlog" => self.backlog.push(card),
            "todo" => self.todo.push(card),
            "in_progress" => self.in_progress.push(card),
            "in_review" => self.in_review.push(card),
            "blocker" => self.blocker.push(card),
            "done" => self.done.push(card),
            _ => {}
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProjectBoard {
    pub project: Project,
    pub columns: BoardColumns,
}

#[derive(Clone, Debug, Serialize)]
pub struct GlobalBoard {
    pub projects: Vec<ProjectBoard>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TokenInfo {
    pub id: String,
    pub name: String,
    pub scope: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}
