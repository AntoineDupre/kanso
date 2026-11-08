//! Background OpenChamber session polling.
//!
//! OpenChamber 2.6 has no SSE stream for session state, so one loop polls every
//! ten seconds. It reads the work from SQLite on each tick, which means a server
//! restart simply resumes from the stored `session_id`/`session_status`.
//!
//! A single `Poller` owns all in-memory observation state and runs its cycles
//! sequentially, so two poll cycles never overlap.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use rusqlite::params;
use serde_json::json;

use super::openchamber::{
    map_observation, session_observation, MappedStatus, SessionState, SessionSummary,
};
use crate::api::model::Task;
use crate::api::now_millis;
use crate::api::tasks::{insert_activity, task_by_ref, task_detail};
use crate::error::{Error, Result};
use crate::server::AppState;

pub const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Three consecutive transport/HTTP failures mark the affected sessions
/// offline and ask for human attention.
pub const OFFLINE_AFTER_FAILURES: u32 = 3;

/// Consecutive-failure counter keyed by a directory or a session id.
#[derive(Clone, Debug, Default)]
pub struct FailureTracker {
    counts: HashMap<String, u32>,
}

impl FailureTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_failure(&mut self, key: &str) -> u32 {
        let count = self.counts.entry(key.to_owned()).or_insert(0);
        *count += 1;
        *count
    }

    pub fn record_success(&mut self, key: &str) {
        self.counts.remove(key);
    }

    pub fn failures(&self, key: &str) -> u32 {
        self.counts.get(key).copied().unwrap_or(0)
    }
}

#[derive(Clone, Debug)]
struct PolledCard {
    id: String,
    session_id: String,
    session_directory: String,
}

pub struct Poller {
    /// Per-directory transport/HTTP failure counts.
    failures: FailureTracker,
    /// A session that disappeared from `session.list` counts separately; a
    /// temporarily incomplete listing should not immediately move a card.
    missing: FailureTracker,
    /// Sessions observed busy while this process has been running. `time.busy`
    /// is the durable equivalent across restarts.
    seen_busy: HashSet<String>,
}

impl Default for Poller {
    fn default() -> Self {
        Self::new()
    }
}

impl Poller {
    pub fn new() -> Self {
        Self {
            failures: FailureTracker::new(),
            missing: FailureTracker::new(),
            seen_busy: HashSet::new(),
        }
    }

    /// One poll cycle: group the live sessions by directory, call
    /// `session.list` once per directory, then apply the mapping to each card.
    pub async fn poll_once(&mut self, state: &AppState) -> Result<()> {
        let Some(openchamber) = state.openchamber.clone() else {
            return Ok(());
        };

        let cards = state
            .db
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT id, session_id, session_directory FROM tasks
                     WHERE state = 'active'
                       AND session_id IS NOT NULL
                       AND session_directory IS NOT NULL
                       AND (
                         session_status IN ('running', 'waiting')
                         OR (session_status = 'idle' AND status = 'in_progress')
                       )
                     ORDER BY id",
                )?;
                let cards = statement
                    .query_map([], |row| {
                        Ok(PolledCard {
                            id: row.get(0)?,
                            session_id: row.get(1)?,
                            session_directory: row.get(2)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(cards)
            })
            .await?;

        let mut by_directory: HashMap<String, Vec<PolledCard>> = HashMap::new();
        for card in cards {
            by_directory
                .entry(card.session_directory.clone())
                .or_default()
                .push(card);
        }

        for (directory, cards) in by_directory {
            match openchamber.list_sessions(&directory).await {
                Ok(sessions) => {
                    self.failures.record_success(&directory);
                    let sessions: HashMap<String, SessionSummary> = sessions
                        .into_iter()
                        .map(|session| (session.id.clone(), session))
                        .collect();
                    for card in cards {
                        self.apply_observation(state, &card, sessions.get(&card.session_id))
                            .await?;
                    }
                }
                Err(error) => {
                    let failures = self.failures.record_failure(&directory);
                    let reason = match &error {
                        super::openchamber::OpenChamberError::Http { status, .. } => {
                            format!("http_{status}")
                        }
                        super::openchamber::OpenChamberError::Protocol(_) => "protocol".to_owned(),
                        super::openchamber::OpenChamberError::Transport(_) => {
                            "transport".to_owned()
                        }
                    };
                    tracing::warn!(
                        %error,
                        directory = %directory,
                        failures,
                        "OpenChamber session poll failed"
                    );
                    if failures >= OFFLINE_AFTER_FAILURES {
                        for card in cards {
                            self.apply_offline(state, &card, &reason).await?;
                        }
                        // The sweep is a fresh start: a session re-linked or
                        // dispatched later must fail three times again.
                        self.failures.record_success(&directory);
                    }
                }
            }
        }

        Ok(())
    }

    async fn apply_observation(
        &mut self,
        state: &AppState,
        card: &PolledCard,
        summary: Option<&SessionSummary>,
    ) -> Result<()> {
        let Some(summary) = summary else {
            let missing = self.missing.record_failure(&card.session_id);
            if missing >= OFFLINE_AFTER_FAILURES {
                self.missing.record_success(&card.session_id);
                self.apply_offline(state, card, "session_missing").await?;
            }
            return Ok(());
        };
        self.missing.record_success(&card.session_id);

        if summary.state == SessionState::Busy {
            self.seen_busy.insert(card.session_id.clone());
        }
        let observation = session_observation(summary, self.seen_busy.contains(&card.session_id));

        let mapped = map_observation(&observation);
        if mapped != MappedStatus::Running {
            // Terminal or unusable sessions stop being polled; drop the
            // per-session memory so it cannot grow for the process lifetime.
            self.seen_busy.remove(&card.session_id);
            self.missing.record_success(&card.session_id);
        }
        match mapped {
            MappedStatus::Running => self.apply_running(state, card).await,
            MappedStatus::Idle => self.apply_idle(state, card).await,
            MappedStatus::Failed(reason) => self.apply_failed(state, card, reason).await,
            MappedStatus::Unknown => Ok(()),
        }
    }

    async fn apply_running(&self, state: &AppState, card: &PolledCard) -> Result<()> {
        let card = card.clone();
        let now = now_millis();
        let task = state
            .db
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let changed = transaction.execute(
                    "UPDATE tasks SET session_status = 'running', updated_at = ?1
                     WHERE id = ?2 AND session_id = ?3 AND session_status <> 'running'",
                    params![now, card.id, card.session_id],
                )?;
                if changed == 0 {
                    return Ok(None);
                }
                insert_activity(
                    &transaction,
                    &card.id,
                    "openchamber",
                    "session_running",
                    &json!({ "session_id": card.session_id, "session_status": "running" }),
                )?;
                let task = task_by_ref(&transaction, &card.id)?
                    .ok_or_else(|| Error::msg("running task missing"))?;
                let detail = task_detail(&transaction, task)?;
                transaction.commit()?;
                Ok(Some(detail.task))
            })
            .await?;
        publish(state, task);
        Ok(())
    }

    async fn apply_idle(&self, state: &AppState, card: &PolledCard) -> Result<()> {
        let card = card.clone();
        let now = now_millis();
        let task = state
            .db
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let Some(current) = task_by_ref(&transaction, &card.id)? else {
                    return Ok(None);
                };
                if current.session_id.as_deref() != Some(card.session_id.as_str())
                    || current.session_status.as_deref() == Some("idle")
                {
                    return Ok(None);
                }

                // Idle is the end of an assistant turn, not the end of the
                // work: the agent may have stopped to ask a question or wait
                // for a reply. Only record the session state; the agent (or
                // the human) moves the card to review explicitly.
                let changed = transaction.execute(
                    "UPDATE tasks SET session_status = 'idle', updated_at = ?1
                     WHERE id = ?2 AND session_id = ?3 AND session_status <> 'idle'",
                    params![now, current.id, card.session_id],
                )?;
                if changed == 0 {
                    return Ok(None);
                }

                insert_activity(
                    &transaction,
                    &current.id,
                    "openchamber",
                    "session_idle",
                    &json!({
                        "session_id": card.session_id,
                        "session_status": "idle",
                    }),
                )?;
                let task = task_by_ref(&transaction, &current.id)?
                    .ok_or_else(|| Error::msg("idle task missing"))?;
                let detail = task_detail(&transaction, task)?;
                transaction.commit()?;
                Ok(Some(detail.task))
            })
            .await?;
        publish(state, task);
        Ok(())
    }

    async fn apply_failed(&self, state: &AppState, card: &PolledCard, reason: &str) -> Result<()> {
        let card = card.clone();
        let reason = reason.to_owned();
        let now = now_millis();
        let task = state
            .db
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let changed = transaction.execute(
                    "UPDATE tasks SET session_status = 'failed', attention = 1, updated_at = ?1
                     WHERE id = ?2 AND session_id = ?3 AND session_status <> 'failed'",
                    params![now, card.id, card.session_id],
                )?;
                if changed == 0 {
                    return Ok(None);
                }
                insert_activity(
                    &transaction,
                    &card.id,
                    "openchamber",
                    "session_failed",
                    &json!({
                        "session_id": card.session_id,
                        "session_status": "failed",
                        "reason": reason,
                    }),
                )?;
                let task = task_by_ref(&transaction, &card.id)?
                    .ok_or_else(|| Error::msg("failed task missing"))?;
                let detail = task_detail(&transaction, task)?;
                transaction.commit()?;
                Ok(Some(detail.task))
            })
            .await?;
        publish(state, task);
        Ok(())
    }

    async fn apply_offline(&self, state: &AppState, card: &PolledCard, reason: &str) -> Result<()> {
        let card = card.clone();
        let reason = reason.to_owned();
        let now = now_millis();
        let task = state
            .db
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let changed = transaction.execute(
                    "UPDATE tasks SET session_status = 'offline', attention = 1, updated_at = ?1
                     WHERE id = ?2 AND session_id = ?3 AND session_status <> 'offline'",
                    params![now, card.id, card.session_id],
                )?;
                if changed == 0 {
                    return Ok(None);
                }
                insert_activity(
                    &transaction,
                    &card.id,
                    "openchamber",
                    "session_offline",
                    &json!({
                        "session_id": card.session_id,
                        "session_status": "offline",
                        "reason": reason,
                    }),
                )?;
                let task = task_by_ref(&transaction, &card.id)?
                    .ok_or_else(|| Error::msg("offline task missing"))?;
                let detail = task_detail(&transaction, task)?;
                transaction.commit()?;
                Ok(Some(detail.task))
            })
            .await?;
        publish(state, task);
        Ok(())
    }
}

fn publish(state: &AppState, task: Option<Task>) {
    if let Some(task) = task {
        state.publish("task.updated", json!({ "task": &task }));
    }
}

/// Start the background poller. Call only when the integration is enabled and
/// only once per process; a second call would create a second cycle loop.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut poller = Poller::new();
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first tick fires immediately; consume it so the first poll runs
        // one interval after startup.
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = poller.poll_once(&state).await {
                tracing::warn!(%error, "OpenChamber poll cycle failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_failures_count_and_reset() {
        let mut tracker = FailureTracker::new();
        assert_eq!(tracker.failures("dir"), 0);
        assert_eq!(tracker.record_failure("dir"), 1);
        assert_eq!(tracker.record_failure("dir"), 2);
        assert_eq!(tracker.record_failure("other"), 1);
        assert_eq!(tracker.failures("dir"), 2);
        assert_eq!(tracker.record_failure("dir"), 3);
        assert!(tracker.record_failure("dir") >= OFFLINE_AFTER_FAILURES);
        tracker.record_success("dir");
        assert_eq!(tracker.failures("dir"), 0);
        assert_eq!(tracker.failures("other"), 1);
    }
}
