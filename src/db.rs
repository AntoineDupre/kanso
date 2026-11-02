use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};

use crate::error::{Error, Result};

/// Versioned schema migrations. Index + 1 is the `PRAGMA user_version` value.
pub const MIGRATIONS: &[&str] = &[include_str!("migrations/v1.sql")];

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
    fn migrates_to_version_one_with_the_full_schema() {
        let (dir, connection) = migrated_database();

        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, 1);

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
                "labels",
                "projects",
                "task_labels",
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

        assert_eq!(migrate(&mut connection).expect("first migration"), 1);
        assert_eq!(migrate(&mut connection).expect("second migration"), 1);

        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, 1);
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
}
