//! Explicit, backup-first schema migration for the event store.
//!
//! Planning never writes, so a caller can always report what would happen
//! before anything does. Applying takes a verified backup first and moves the
//! whole chain inside one transaction, so a failure leaves the original store
//! openable at its original version.

use crate::{
    event::StoreError,
    sqlite::{read_schema_version, write_schema_version, SCHEMA_VERSION},
};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::{path::Path, time::Duration};

/// One step from one schema version to the next.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Migration {
    pub from: u32,
    pub to: u32,
    /// What the step does, in one line, for the report.
    pub description: &'static str,
    /// What the step cannot carry forward. Empty means nothing is lost.
    pub loss: &'static [&'static str],
    pub sql: &'static str,
}

/// Every known step, in order.
///
/// Version 1 is the baseline shape, so there is nothing to migrate yet; this
/// module is the mechanism, not a guess at the first change.
static MIGRATIONS: &[Migration] = &[];

/// What a migration would do. An empty `steps` means the store is current.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationPlan {
    pub current: u32,
    pub target: u32,
    pub steps: Vec<Migration>,
    pub loss: Vec<&'static str>,
}

impl MigrationPlan {
    pub fn is_current(&self) -> bool {
        self.steps.is_empty()
    }
}

/// Report the planned migration without touching the store.
pub fn plan(path: impl AsRef<Path>) -> Result<MigrationPlan, StoreError> {
    plan_with(path.as_ref(), MIGRATIONS)
}

/// Take a verified backup, then apply the plan in one transaction.
///
/// A store that is already current is left alone and no backup is written.
pub fn apply(
    path: impl AsRef<Path>,
    backup: impl AsRef<Path>,
) -> Result<MigrationPlan, StoreError> {
    apply_with(path.as_ref(), backup.as_ref(), MIGRATIONS)
}

fn plan_with(path: &Path, migrations: &[Migration]) -> Result<MigrationPlan, StoreError> {
    let connection = open_existing(path)?;
    let current = current_version(&connection)?;
    let target = migrations.last().map_or(SCHEMA_VERSION, |step| step.to);

    let mut steps = Vec::new();
    let mut at = current;
    while at < target {
        let step = migrations
            .iter()
            .find(|step| step.from == at)
            .ok_or_else(|| {
                StoreError::Storage(format!(
                    "no migration path from schema version {at} to {target}"
                ))
            })?;
        at = step.to;
        steps.push(*step);
    }
    if current > target {
        return Err(StoreError::SchemaTooNew {
            current,
            supported: target,
        });
    }

    let loss = steps.iter().flat_map(|step| step.loss).copied().collect();
    Ok(MigrationPlan {
        current,
        target,
        steps,
        loss,
    })
}

fn apply_with(
    path: &Path,
    backup: &Path,
    migrations: &[Migration],
) -> Result<MigrationPlan, StoreError> {
    let plan = plan_with(path, migrations)?;
    if plan.is_current() {
        return Ok(plan);
    }

    let mut connection = open_writable(path)?;
    write_verified_backup(&connection, backup, plan.current)?;

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    for step in &plan.steps {
        transaction.execute_batch(step.sql).map_err(storage)?;
    }
    write_schema_version(&transaction, plan.target)?;
    transaction.commit().map_err(storage)?;
    Ok(plan)
}

/// Copy the store with SQLite's own consistent-snapshot command, then prove the
/// copy is readable and carries the version we are migrating away from.
fn write_verified_backup(
    connection: &Connection,
    backup: &Path,
    expected: u32,
) -> Result<(), StoreError> {
    if backup.exists() {
        return Err(StoreError::Storage(format!(
            "backup path {} already exists",
            backup.display()
        )));
    }
    connection
        .execute("VACUUM INTO ?1", [path_str(backup)?])
        .map_err(storage)?;

    let copy = open_existing(backup)?;
    let integrity: String = copy
        .pragma_query_value(None, "integrity_check", |row| row.get(0))
        .map_err(storage)?;
    if integrity != "ok" {
        return Err(StoreError::Storage(format!(
            "backup failed its integrity check: {integrity}"
        )));
    }
    if current_version(&copy)? != expected {
        return Err(StoreError::Storage(
            "backup does not carry the source schema version".into(),
        ));
    }
    Ok(())
}

/// A store written before stamping began already has the baseline shape.
fn current_version(connection: &Connection) -> Result<u32, StoreError> {
    Ok(read_schema_version(connection)?.max(1))
}

fn open_existing(path: &Path) -> Result<Connection, StoreError> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| StoreError::Storage(format!("{}: {error}", path.display())))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(storage)?;
    Ok(connection)
}

fn open_writable(path: &Path) -> Result<Connection, StoreError> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|error| StoreError::Storage(format!("{}: {error}", path.display())))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(storage)?;
    Ok(connection)
}

fn path_str(path: &Path) -> Result<&str, StoreError> {
    path.to_str()
        .ok_or_else(|| StoreError::Storage("path is not valid UTF-8".into()))
}

fn storage(error: rusqlite::Error) -> StoreError {
    StoreError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{CorrelationId, Principal, SessionId},
        event::{EventEnvelope, EventPayload, EventStore, SchemaVersion, StreamVersion},
        sqlite::{Durability, SqliteEventStore},
    };
    use serde_json::Value;
    use std::{fs, path::PathBuf};
    use uuid::Uuid;

    // Version 1 is the baseline, so the real registry is empty. These stand in
    // for a future step and exercise the engine end to end.
    static GOOD: &[Migration] = &[Migration {
        from: 1,
        to: 2,
        description: "record who closed a session",
        loss: &["sessions.closed_by is empty for sessions that existed before"],
        sql: "ALTER TABLE sessions ADD COLUMN closed_by TEXT;",
    }];

    static BROKEN: &[Migration] = &[Migration {
        from: 1,
        to: 2,
        description: "a step that cannot run",
        loss: &[],
        sql: "ALTER TABLE sessions ADD COLUMN durability TEXT;",
    }];

    fn store_path() -> PathBuf {
        std::env::temp_dir().join(format!("arsy-migrate-test-{}", Uuid::new_v4()))
    }

    fn seeded_store() -> PathBuf {
        let path = store_path();
        drop(SqliteEventStore::open(&path, Durability::Normal).unwrap());
        path
    }

    fn cleanup(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn planning_reports_steps_and_loss_without_writing() {
        let path = seeded_store();
        let before = fs::metadata(&path).unwrap().len();

        let plan = plan_with(&path, GOOD).unwrap();

        assert_eq!(plan.current, 1);
        assert_eq!(plan.target, 2);
        assert_eq!(plan.steps, GOOD);
        assert_eq!(plan.loss.len(), 1);
        assert_eq!(fs::metadata(&path).unwrap().len(), before);
        assert_eq!(
            read_schema_version(&open_existing(&path).unwrap()).unwrap(),
            1
        );
        cleanup(&path);
    }

    #[test]
    fn a_current_store_needs_no_backup_and_no_write() {
        let path = seeded_store();
        let backup = store_path();

        let plan = apply_with(&path, &backup, &[]).unwrap();

        assert!(plan.is_current());
        assert!(!backup.exists());
        cleanup(&path);
    }

    #[test]
    fn applying_backs_up_first_and_moves_the_version() {
        let path = seeded_store();
        let backup = store_path();

        apply_with(&path, &backup, GOOD).unwrap();

        assert_eq!(
            read_schema_version(&open_existing(&path).unwrap()).unwrap(),
            2
        );
        assert_eq!(
            read_schema_version(&open_existing(&backup).unwrap()).unwrap(),
            1
        );
        cleanup(&path);
        cleanup(&backup);
    }

    #[test]
    fn a_failed_step_leaves_the_store_at_its_old_version() {
        let path = seeded_store();
        let backup = store_path();

        let error = apply_with(&path, &backup, BROKEN).expect_err("a duplicate column must fail");
        assert!(matches!(error, StoreError::Storage(_)), "{error}");

        assert_eq!(
            read_schema_version(&open_existing(&path).unwrap()).unwrap(),
            1
        );
        // The original still opens, which is the promise a failed migration makes.
        drop(SqliteEventStore::open(&path, Durability::Normal).unwrap());
        cleanup(&path);
        cleanup(&backup);
    }

    #[test]
    fn events_survive_a_migration_unchanged() {
        let path = seeded_store();
        let backup = store_path();
        let store = SqliteEventStore::open(&path, Durability::Normal).unwrap();
        let session = SessionId::new();
        let events: Vec<_> = (1..=32)
            .map(|sequence| {
                EventEnvelope::new(
                    session,
                    sequence,
                    Principal::System,
                    None,
                    CorrelationId::new(),
                    SchemaVersion(1),
                    "test.event",
                    EventPayload::Inline { data: Value::Null },
                )
            })
            .collect();
        store
            .append(session, StreamVersion(0), events.clone())
            .unwrap();

        apply_with(&path, &backup, GOOD).unwrap();

        assert_eq!(store.read(session, 1, usize::MAX).unwrap(), events);
        cleanup(&path);
        cleanup(&backup);
    }

    #[test]
    fn an_existing_backup_path_is_never_overwritten() {
        let path = seeded_store();
        let backup = store_path();
        fs::write(&backup, b"do not clobber me").unwrap();

        let error =
            apply_with(&path, &backup, GOOD).expect_err("an occupied backup path must fail");
        assert!(matches!(error, StoreError::Storage(_)), "{error}");

        assert_eq!(fs::read(&backup).unwrap(), b"do not clobber me");
        assert_eq!(
            read_schema_version(&open_existing(&path).unwrap()).unwrap(),
            1
        );
        cleanup(&path);
        cleanup(&backup);
    }
}
