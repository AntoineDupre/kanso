use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::error::{Error, Result};

type SecretChanges = Mutex<HashSet<(String, String)>>;

/// Versioned schema migrations. Index + 1 is the `PRAGMA user_version` value.
pub const MIGRATIONS: &[&str] = &[
    include_str!("migrations/v1.sql"),
    include_str!("migrations/v2.sql"),
    include_str!("migrations/v3.sql"),
    include_str!("migrations/v4.sql"),
    include_str!("migrations/v5.sql"),
    include_str!("migrations/v6.sql"),
];

/// Open the database and apply the connection-level pragmas.
pub fn open(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")?;

    let journal_mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(Error::msg(format!(
            "could not enable WAL journal mode (got {journal_mode:?})"
        )));
    }

    Ok(connection)
}

/// Apply every migration newer than the database's `user_version`.
///
/// Each migration and its version bump run in one immediate transaction, so a
/// failure leaves the database at the previous version and concurrent starters
/// serialize instead of racing. Running this twice is a no-op.
pub fn migrate(connection: &mut Connection) -> Result<u32> {
    apply(connection, MIGRATIONS)
}

/// SQLite access used from axum handlers.
///
/// All calls run on Tokio's blocking pool with a single serialized connection.
/// Holding the `std::sync::Mutex` inside the blocking closure means the async
/// runtime never waits on SQLite, and no guard is ever held across `.await`.
#[derive(Clone)]
pub struct Database {
    path: PathBuf,
    connection: Arc<Mutex<Connection>>,
    secrets: Arc<crate::secret_memory::SecretMemory>,
    secret_changes: Arc<SecretChanges>,
}

impl Database {
    /// Open, configure and migrate the database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let mut connection = open(path)?;
        migrate(&mut connection)?;
        Self::from_connection(path.to_path_buf(), connection)
    }

    /// Wrap an already opened and migrated connection.
    pub fn from_connection(path: PathBuf, connection: Connection) -> Result<Self> {
        connection.execute("UPDATE secret_requests SET state = 'invalidated', verifier = NULL, invalidation_reason = 'restart' WHERE state IN ('waiting', 'ready')", [])?;
        Ok(Self {
            path,
            connection: Arc::new(Mutex::new(connection)),
            secrets: Arc::new(crate::secret_memory::SecretMemory::default()),
            secret_changes: Arc::new(Mutex::new(std::collections::HashSet::new())),
        })
    }

    pub fn secrets(&self) -> Arc<crate::secret_memory::SecretMemory> {
        Arc::clone(&self.secrets)
    }

    pub(crate) fn take_secret_changes(&self) -> Vec<(String, String)> {
        self.secret_changes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain()
            .collect()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run one synchronous SQLite operation on the blocking pool.
    ///
    /// The closure receives `&mut Connection` so it can start transactions. It
    /// must be `'static` because it moves to another thread.
    pub async fn call<F, T>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let connection = Arc::clone(&self.connection);
        let secrets = Arc::clone(&self.secrets);
        let secret_changes = Arc::clone(&self.secret_changes);
        tokio::task::spawn_blocking(move || {
            let mut connection = connection.lock().unwrap_or_else(|error| error.into_inner());
            // The same connection lock serializes cleanup with all task/token changes.
            cleanup_secrets(&connection, &secrets, &secret_changes)?;
            let result = operation(&mut connection);
            let cleanup = cleanup_secrets(&connection, &secrets, &secret_changes);
            match result {
                Err(error) => Err(error),
                Ok(value) => {
                    cleanup?;
                    Ok(value)
                }
            }
        })
        .await
        .map_err(|error| Error::msg(format!("database worker failed: {error}")))?
    }
}

/// Called only while holding the serialized connection lock. Database triggers
/// close requests when cards/tokens change; remove their volatile values before
/// releasing that same lock to another operation.
fn cleanup_secrets(
    connection: &Connection,
    secrets: &crate::secret_memory::SecretMemory,
    changes: &SecretChanges,
) -> Result<()> {
    let mut expired = connection.prepare(
        "UPDATE secret_requests SET state='expired',verifier=NULL
         WHERE state IN ('waiting','ready')
           AND (deadline<=?1 OR (value_deadline IS NOT NULL AND value_deadline<=?1))
         RETURNING id,task_id",
    )?;
    let mut changed = expired
        .query_map([crate::api::now_millis()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<HashSet<_>>>()?;

    let mut statement =
        connection.prepare("SELECT id FROM secret_requests WHERE state IN ('waiting','ready')")?;
    let live = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    for id in secrets.retain_live(&live) {
        if let Some(task_id) = connection
            .query_row(
                "SELECT task_id FROM secret_requests WHERE id=?1",
                [&id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            changed.insert((id, task_id));
        }
    }
    for id in secrets.expire(std::time::Instant::now()) {
        // Monotonic expiry remains authoritative if the wall clock moves back;
        // never leave a phantom ready handoff with no retrievable value.
        if let Some(task_id) = connection
            .query_row(
                "UPDATE secret_requests SET state='expired',verifier=NULL
             WHERE id=?1 AND state IN ('waiting','ready') RETURNING task_id",
                [&id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            changed.insert((id, task_id));
        }
    }
    changes
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .extend(changed);
    Ok(())
}

fn apply(connection: &mut Connection, migrations: &[&str]) -> Result<u32> {
    let current: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let latest = migrations.len() as u32;

    if current > latest {
        return Err(Error::msg(format!(
            "database schema version {current} is newer than this binary supports ({latest})"
        )));
    }

    for (index, sql) in migrations.iter().enumerate() {
        let version = index as u32 + 1;
        if version <= current {
            continue;
        }

        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Another process may have applied this migration while we waited.
        let applied: u32 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if applied > latest {
            return Err(Error::msg(format!(
                "database schema version {applied} is newer than this binary supports ({latest})"
            )));
        }
        if applied >= version {
            transaction.commit()?;
            continue;
        }

        transaction.execute_batch(sql)?;
        transaction.pragma_update(None, "user_version", version)?;
        transaction.commit()?;
        tracing::info!(version, "applied database migration");
    }

    Ok(latest)
}

/// Run SQLite's `quick_check`; a missing database is unhealthy, not created.
pub fn quick_check(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    Connection::open(path)
        .and_then(|connection| {
            connection.busy_timeout(Duration::from_secs(5))?;
            connection.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
        })
        .map(|result| result == "ok")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn migrated_database() -> (TempDir, Connection) {
        let dir = TempDir::new();
        let path = dir.path().join("kanso.db");
        let mut connection = open(&path).expect("open database");
        migrate(&mut connection).expect("migrate database");
        (dir, connection)
    }

    fn table_exists(connection: &Connection, name: &str) -> bool {
        connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |_| Ok(()),
            )
            .is_ok()
    }

    #[test]
    fn migrates_to_the_latest_version_with_the_full_schema() {
        let (dir, connection) = migrated_database();

        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, 6);

        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                 ORDER BY name",
            )
            .expect("prepare table query");
        let tables: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .expect("query tables")
            .collect::<rusqlite::Result<_>>()
            .expect("collect tables");
        assert_eq!(
            tables,
            [
                "activity",
                "checklist",
                "comments",
                "dependencies",
                "github_repositories",
                "labels",
                "projects",
                "secret_requests",
                "task_labels",
                "task_provenance",
                "tasks",
                "tokens",
                "ui_sessions",
            ]
        );

        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'index' AND name NOT LIKE 'sqlite_%'",
            )
            .expect("prepare index query");
        let indexes: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .expect("query indexes")
            .collect::<rusqlite::Result<_>>()
            .expect("collect indexes");
        for expected in [
            "activity_task",
            "checklist_task",
            "comments_task",
            "dependencies_rev",
            "tasks_board",
            "tasks_session",
        ] {
            assert!(indexes.iter().any(|name| name == expected), "{expected}");
        }

        assert!(quick_check(&dir.path().join("kanso.db")));
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = TempDir::new();
        let path = dir.path().join("kanso.db");
        let mut connection = open(&path).expect("open database");

        assert_eq!(migrate(&mut connection).expect("first migration"), 6);
        assert_eq!(migrate(&mut connection).expect("second migration"), 6);

        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, 6);
    }

    #[test]
    fn upgrade_from_v1_keeps_data_and_adds_the_checklist_index() {
        let dir = TempDir::new();
        let path = dir.path().join("kanso.db");
        let mut connection = open(&path).expect("open database");
        assert_eq!(
            apply(&mut connection, &MIGRATIONS[..1]).expect("migrate to v1"),
            1
        );

        connection
            .execute_batch(
                "INSERT INTO projects (id, key, name, position, created_at, updated_at)
                   VALUES ('p1', 'ALP', 'Alpha', 1.0, 1, 1);
                 INSERT INTO tasks
                   (id, project_id, number, title, status, rank, created_at, updated_at)
                   VALUES ('t1', 'p1', 1, 'First', 'todo', 1.0, 1, 1);
                 INSERT INTO checklist (id, task_id, text, done, position)
                   VALUES ('c1', 't1', 'step', 1, 1.0);",
            )
            .expect("seed v1 rows");

        assert_eq!(migrate(&mut connection).expect("migrate to v6"), 6);

        let mut statement = connection
            .prepare("PRAGMA index_info('checklist_task')")
            .expect("prepare index info");
        let columns: Vec<String> = statement
            .query_map([], |row| row.get(2))
            .expect("query index info")
            .collect::<rusqlite::Result<_>>()
            .expect("collect index info");
        assert_eq!(columns, ["task_id"]);

        let mut statement = connection
            .prepare("PRAGMA index_list('checklist')")
            .expect("prepare index list");
        let indexes: Vec<String> = statement
            .query_map([], |row| row.get(1))
            .expect("query index list")
            .collect::<rusqlite::Result<_>>()
            .expect("collect index list");
        assert!(indexes.iter().any(|name| name == "checklist_task"));

        let row: (String, String, i64) = connection
            .query_row(
                "SELECT id, task_id, done FROM checklist WHERE id = 'c1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read seeded checklist row");
        assert_eq!(row, ("c1".to_owned(), "t1".to_owned(), 1));
    }

    #[test]
    fn upgrade_from_v2_keeps_comments_and_adds_the_reply_delivery_columns() {
        let dir = TempDir::new();
        let path = dir.path().join("kanso.db");
        let mut connection = open(&path).expect("open database");
        assert_eq!(
            apply(&mut connection, &MIGRATIONS[..2]).expect("migrate to v2"),
            2
        );

        connection
            .execute_batch(
                "INSERT INTO projects (id, key, name, position, created_at, updated_at)
                   VALUES ('p1', 'ALP', 'Alpha', 1.0, 1, 1);
                 INSERT INTO tasks
                   (id, project_id, number, title, status, rank, created_at, updated_at)
                   VALUES ('t1', 'p1', 1, 'First', 'blocker', 1.0, 1, 1);
                 INSERT INTO comments (id, task_id, author, body, created_at)
                   VALUES ('c1', 't1', 'ui', 'existing', 1);",
            )
            .expect("seed v2 rows");

        assert_eq!(migrate(&mut connection).expect("migrate to v6"), 6);

        let mut statement = connection
            .prepare("PRAGMA table_info('comments')")
            .expect("prepare table info");
        let columns: Vec<String> = statement
            .query_map([], |row| row.get(1))
            .expect("query table info")
            .collect::<rusqlite::Result<_>>()
            .expect("collect table info");
        for expected in [
            "delivery_status",
            "delivery_error",
            "delivery_session_id",
            "delivery_session_directory",
            "reply_prompt",
        ] {
            assert!(columns.iter().any(|name| name == expected), "{expected}");
        }

        let row: (String, Option<String>, Option<String>) = connection
            .query_row(
                "SELECT body, delivery_status, reply_prompt FROM comments WHERE id = 'c1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read seeded comment");
        assert_eq!(row, ("existing".to_owned(), None, None));
    }

    #[test]
    fn upgrade_from_v4_preserves_tasks_and_enforces_import_identity() {
        let dir = TempDir::new();
        let mut connection = open(&dir.path().join("kanso.db")).unwrap();
        apply(&mut connection, &MIGRATIONS[..4]).unwrap();
        connection.execute_batch("INSERT INTO projects (id,key,name,position,created_at,updated_at) VALUES ('p','ABC','Existing',1,1,1); INSERT INTO tasks (id,project_id,number,title,status,rank,created_at,updated_at) VALUES ('t','p',1,'Existing','todo',1,1,1), ('u','p',2,'Second','todo',2,1,1);").unwrap();
        assert_eq!(migrate(&mut connection).unwrap(), 6);
        assert_eq!(
            connection
                .query_row("SELECT title FROM tasks WHERE id='t'", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "Existing"
        );
        connection.execute("INSERT INTO task_provenance VALUES ('t','p','fixture','stable','issue','https://example.com')", []).unwrap();
        assert!(connection.execute("INSERT INTO task_provenance VALUES ('u','p','fixture','stable','issue','https://example.com')", []).is_err());
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let dir = TempDir::new();
        let mut connection = open(&dir.path().join("kanso.db")).expect("open database");
        connection
            .pragma_update(None, "user_version", 99)
            .expect("set user_version");

        let error = migrate(&mut connection).unwrap_err().to_string();
        assert!(error.contains("newer"), "{error}");
    }

    #[test]
    fn failed_migration_rolls_back() {
        let dir = TempDir::new();
        let mut connection = open(&dir.path().join("kanso.db")).expect("open database");

        let error = apply(
            &mut connection,
            &["CREATE TABLE first (id TEXT); CREATE TABLE broken (;"],
        )
        .unwrap_err();
        assert!(!error.to_string().is_empty());

        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, 0);
        assert!(!table_exists(&connection, "first"));
    }

    #[test]
    fn foreign_keys_are_enabled() {
        let (_dir, connection) = migrated_database();
        let enabled: i64 = connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("read foreign_keys");
        assert_eq!(enabled, 1);
    }

    #[test]
    fn journal_mode_is_wal() {
        let (_dir, connection) = migrated_database();
        let mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("read journal_mode");
        assert_eq!(mode.to_ascii_lowercase(), "wal");
    }

    #[test]
    fn quick_check_rejects_a_missing_database() {
        let dir = TempDir::new();
        assert!(!quick_check(&dir.path().join("missing.db")));
        assert!(!dir.path().join("missing.db").exists());
    }

    #[tokio::test]
    async fn database_calls_run_off_the_async_worker() {
        let dir = TempDir::new();
        let path = dir.path().join("kanso.db");
        let database = Database::open(&path).expect("open database");
        let caller = std::thread::current().id();

        let (value, worker) = database
            .call(move |connection| {
                let value = connection
                    .query_row("SELECT 41 + 1", [], |row| row.get::<_, i64>(0))
                    .map_err(Error::from)?;
                Ok((value, std::thread::current().id()))
            })
            .await
            .expect("run query");
        assert_eq!(value, 42);
        assert_ne!(caller, worker);
        assert_eq!(database.path(), path);
    }

    #[tokio::test]
    async fn concurrent_database_calls_are_serialized() {
        let dir = TempDir::new();
        let database = Database::open(&dir.path().join("kanso.db")).expect("open database");

        let mut tasks = Vec::new();
        for _ in 0..16 {
            let database = database.clone();
            tasks.push(tokio::spawn(async move {
                database
                    .call(|connection| {
                        connection
                            .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                            .map_err(Error::from)
                    })
                    .await
                    .expect("query")
            }));
        }
        for task in tasks {
            assert_eq!(task.await.expect("join"), 1);
        }
    }
}
