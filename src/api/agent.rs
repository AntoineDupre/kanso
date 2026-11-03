use std::cmp::Ordering;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::OptionalExtension;
use serde::Deserialize;

use super::model::{Task, TaskDetail, TASK_COLUMNS};
use super::tasks::{claim_in_transaction, task_by_ref, task_detail, ClaimOutcome};
use super::{now_millis, parse_flag, ApiError, ApiQuery, Auth};
use crate::auth::{self, IdentityKind};
use crate::server::AppState;

#[derive(Deserialize)]
pub struct NextQuery {
    project: Option<String>,
    claim: Option<String>,
}

fn priority_order(priority: &str) -> u8 {
    match priority {
        "high" => 0,
        "normal" => 1,
        "low" => 2,
        _ => 3,
    }
}

#[derive(Clone, Copy, Debug)]
struct NextKey<'a> {
    priority: &'a str,
    due_date: Option<&'a str>,
    rank: f64,
    number: i64,
}

impl NextKey<'_> {
    /// `high` > `normal` > `low`, then earliest due date with NULLs last, then
    /// rank, then card number.
    fn compare(&self, other: &Self) -> Ordering {
        priority_order(self.priority)
            .cmp(&priority_order(other.priority))
            .then_with(|| match (self.due_date, other.due_date) {
                (Some(left), Some(right)) => left.cmp(right),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            })
            .then_with(|| {
                self.rank
                    .partial_cmp(&other.rank)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| self.number.cmp(&other.number))
    }
}

fn next_key(task: &Task) -> NextKey<'_> {
    NextKey {
        priority: &task.priority,
        due_date: task.due_date.as_deref(),
        rank: task.rank,
        number: task.number,
    }
}

fn compare_tasks(left: &Task, right: &Task) -> Ordering {
    next_key(left).compare(&next_key(right))
}

enum NextOutcome {
    Empty,
    Found(Box<TaskDetail>),
    Claimed(Box<TaskDetail>),
    UnknownProject,
}

pub async fn next(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<NextQuery>,
) -> Result<Response, ApiError> {
    let claim = parse_flag(query.claim.as_deref(), false)?;
    // `claim=1` mutates, so a cookie-authenticated GET needs the same CSRF
    // check as any other mutation. Bearer tokens are exempt like elsewhere.
    if claim
        && auth.0.kind == IdentityKind::Ui
        && !auth::csrf_origin_allowed(&headers, state.auth.public_url())
    {
        return Err(ApiError::csrf());
    }
    let project = query.project;
    let actor = auth.0.actor().to_owned();
    let claimed_by = auth.0.name.clone();
    let now = now_millis();

    let outcome = state
        .db
        .call(move |connection| {
            let transaction = connection.transaction()?;

            let project_id = if let Some(project) = &project {
                let found = transaction
                    .query_row(
                        "SELECT id FROM projects WHERE id = ?1 OR key = ?1 COLLATE NOCASE",
                        [project],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                match found {
                    Some(id) => Some(id),
                    None => return Ok(NextOutcome::UnknownProject),
                }
            } else {
                None
            };

            let mut sql = format!(
                "SELECT {TASK_COLUMNS} FROM tasks t JOIN projects p ON p.id = t.project_id
                 WHERE t.state = 'active' AND t.status IN ('backlog', 'todo')
                   AND p.archived_at IS NULL
                   AND t.claimed_by IS NULL
                   AND NOT EXISTS (
                     SELECT 1 FROM dependencies d
                     JOIN tasks blocked ON blocked.id = d.blocked_by
                     WHERE d.task_id = t.id AND blocked.status <> 'done'
                   )"
            );
            if project_id.is_some() {
                sql.push_str(" AND t.project_id = ?1");
            }
            let mut statement = transaction.prepare(&sql)?;
            let mut candidates = if let Some(project_id) = &project_id {
                statement
                    .query_map([project_id], Task::from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            } else {
                statement
                    .query_map([], Task::from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            drop(statement);
            candidates.sort_by(compare_tasks);

            if claim {
                for task in &candidates {
                    match claim_in_transaction(&transaction, task, &claimed_by, &actor, now)? {
                        ClaimOutcome::Claimed => {
                            let task = task_by_ref(&transaction, &task.id)?
                                .ok_or_else(|| crate::error::Error::msg("claimed task missing"))?;
                            let detail = task_detail(&transaction, task)?;
                            transaction.commit()?;
                            return Ok(NextOutcome::Claimed(Box::new(detail)));
                        }
                        // Another writer got there first; try the next card.
                        ClaimOutcome::AlreadyClaimed
                        | ClaimOutcome::NotClaimable
                        | ClaimOutcome::DependencyUnmet => continue,
                    }
                }
                return Ok(NextOutcome::Empty);
            }

            let Some(task) = candidates.into_iter().next() else {
                return Ok(NextOutcome::Empty);
            };
            let task = task_by_ref(&transaction, &task.id)?
                .ok_or_else(|| crate::error::Error::msg("next task missing"))?;
            let detail = task_detail(&transaction, task)?;
            transaction.commit()?;
            Ok(NextOutcome::Found(Box::new(detail)))
        })
        .await?;

    match outcome {
        NextOutcome::Empty => Ok(StatusCode::NO_CONTENT.into_response()),
        NextOutcome::Found(detail) => Ok(Json(*detail).into_response()),
        NextOutcome::Claimed(detail) => {
            state.publish("task.claimed", serde_json::json!({ "task": &detail.task }));
            Ok(Json(*detail).into_response())
        }
        NextOutcome::UnknownProject => Err(ApiError::not_found("project not found")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_orders_by_priority_due_date_rank_then_number() {
        let key = |priority: &'static str, due_date: Option<&'static str>, rank, number| NextKey {
            priority,
            due_date,
            rank,
            number,
        };

        // Priority dominates everything.
        assert_eq!(
            key("high", None, 99.0, 99).compare(&key("normal", Some("2020-01-01"), 1.0, 1)),
            Ordering::Less
        );
        // Earlier due date wins; NULL is last.
        assert_eq!(
            key("normal", Some("2026-01-01"), 9.0, 9).compare(&key(
                "normal",
                Some("2026-01-02"),
                1.0,
                1
            )),
            Ordering::Less
        );
        assert_eq!(
            key("normal", Some("2026-01-02"), 9.0, 9).compare(&key("normal", None, 1.0, 1)),
            Ordering::Less
        );
        assert_eq!(
            key("normal", None, 1.0, 1).compare(&key("normal", Some("2026-01-02"), 9.0, 9)),
            Ordering::Greater
        );
        // Then rank, then number.
        assert_eq!(
            key("low", None, 1.0, 9).compare(&key("low", None, 2.0, 1)),
            Ordering::Less
        );
        assert_eq!(
            key("low", None, 1.0, 2).compare(&key("low", None, 1.0, 3)),
            Ordering::Less
        );
        assert_eq!(
            key("low", None, 1.0, 1).compare(&key("low", None, 1.0, 1)),
            Ordering::Equal
        );
    }

    #[test]
    fn priorities_are_high_normal_low() {
        assert!(priority_order("high") < priority_order("normal"));
        assert!(priority_order("normal") < priority_order("low"));
    }
}
