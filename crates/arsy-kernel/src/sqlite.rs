use crate::{
    domain::{Principal, SessionId},
    event::{EventEnvelope, EventStore, StoreError, StreamVersion},
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    stream_id TEXT PRIMARY KEY,
    version INTEGER NOT NULL CHECK (version >= 0),
    durability TEXT NOT NULL CHECK (durability IN ('memory', 'normal', 'strict'))
) STRICT;

CREATE TABLE IF NOT EXISTS events (
    stream_id TEXT NOT NULL REFERENCES sessions(stream_id),
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    event_id TEXT NOT NULL UNIQUE,
    actor_json TEXT NOT NULL,
    correlation_id TEXT NOT NULL,
    occurred_at_ms INTEGER NOT NULL,
    envelope_json TEXT NOT NULL,
    PRIMARY KEY (stream_id, sequence)
) WITHOUT ROWID, STRICT;

CREATE INDEX IF NOT EXISTS events_actor_time
    ON events(actor_json, occurred_at_ms);
CREATE INDEX IF NOT EXISTS events_correlation
    ON events(correlation_id, stream_id, sequence);
"#;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    Memory,
    Normal,
    Strict,
}

impl Durability {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Normal => "normal",
            Self::Strict => "strict",
        }
    }

    const fn synchronous(self) -> &'static str {
        match self {
            Self::Memory => "OFF",
            Self::Normal => "NORMAL",
            Self::Strict => "FULL",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "memory" => Ok(Self::Memory),
            "normal" => Ok(Self::Normal),
            "strict" => Ok(Self::Strict),
            _ => Err(StoreError::Storage(format!("unknown durability {value}"))),
        }
    }
}

pub struct SqliteEventStore {
    path: PathBuf,
    writer: Mutex<Connection>,
    durability: Durability,
}

impl SqliteEventStore {
    pub fn open(path: impl AsRef<Path>, durability: Durability) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let connection = Connection::open(&path).map_err(storage)?;
        configure(&connection, durability)?;
        connection.execute_batch(SCHEMA).map_err(storage)?;
        Ok(Self {
            path,
            writer: Mutex::new(connection),
            durability,
        })
    }

    pub fn session_durability(&self, stream: SessionId) -> Result<Option<Durability>, StoreError> {
        let connection = self.reader()?;
        connection
            .query_row(
                "SELECT durability FROM sessions WHERE stream_id = ?1",
                [stream.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(storage)?
            .map(|value| Durability::parse(&value))
            .transpose()
    }

    pub fn read_by_actor(
        &self,
        actor: &Principal,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        let connection = self.reader()?;
        let actor = serde_json::to_string(actor).map_err(serialization)?;
        let mut statement = connection
            .prepare(
                "SELECT envelope_json FROM events
                 WHERE actor_json = ?1
                 ORDER BY occurred_at_ms, stream_id, sequence
                 LIMIT ?2",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![actor, limit_i64(limit)], |row| {
                row.get::<_, String>(0)
            })
            .map_err(storage)?;
        decode_rows(rows)
    }

    fn reader(&self) -> Result<Connection, StoreError> {
        let connection = Connection::open(&self.path).map_err(storage)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(storage)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(storage)?;
        Ok(connection)
    }
}

impl EventStore for SqliteEventStore {
    fn current_version(&self, stream: SessionId) -> Result<StreamVersion, StoreError> {
        let connection = self.reader()?;
        let version = connection
            .query_row(
                "SELECT version FROM sessions WHERE stream_id = ?1",
                [stream.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(storage)?
            .unwrap_or_default();
        Ok(StreamVersion(version.try_into().map_err(|_| {
            StoreError::Storage("negative stream version".into())
        })?))
    }

    fn append(
        &self,
        stream: SessionId,
        expected: StreamVersion,
        events: Vec<EventEnvelope>,
    ) -> Result<StreamVersion, StoreError> {
        let mut encoded = Vec::with_capacity(events.len());
        for (offset, event) in events.iter().enumerate() {
            let sequence = expected
                .0
                .checked_add(offset as u64 + 1)
                .ok_or(StoreError::SequenceOverflow)?;
            event.validate_for_append(stream, sequence)?;
            encoded.push((
                event,
                serde_json::to_string(event).map_err(serialization)?,
                serde_json::to_string(&event.actor).map_err(serialization)?,
            ));
        }

        let mut connection = self
            .writer
            .lock()
            .map_err(|_| StoreError::Storage("SQLite writer lock poisoned".into()))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let stored = transaction
            .query_row(
                "SELECT version, durability FROM sessions WHERE stream_id = ?1",
                [stream.to_string()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(storage)?;
        let actual = match stored {
            Some((version, stored_durability)) => {
                if stored_durability != self.durability.as_str() {
                    return Err(StoreError::Storage(format!(
                        "session durability is {stored_durability}, writer requested {}",
                        self.durability.as_str()
                    )));
                }
                StreamVersion(
                    version
                        .try_into()
                        .map_err(|_| StoreError::Storage("negative stream version".into()))?,
                )
            }
            None => {
                transaction
                    .execute(
                        "INSERT INTO sessions(stream_id, version, durability) VALUES (?1, 0, ?2)",
                        params![stream.to_string(), self.durability.as_str()],
                    )
                    .map_err(storage)?;
                StreamVersion(0)
            }
        };
        if actual != expected {
            return Err(StoreError::Conflict { expected, actual });
        }

        for (event, envelope_json, actor_json) in encoded {
            transaction
                .execute(
                    "INSERT INTO events(
                        stream_id, sequence, event_id, actor_json, correlation_id,
                        occurred_at_ms, envelope_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        stream.to_string(),
                        integer(event.sequence)?,
                        event.id.to_string(),
                        actor_json,
                        event.correlation.to_string(),
                        integer(event.occurred_at_ms)?,
                        envelope_json,
                    ],
                )
                .map_err(storage)?;
        }
        let new_version = expected
            .0
            .checked_add(events.len() as u64)
            .ok_or(StoreError::SequenceOverflow)?;
        transaction
            .execute(
                "UPDATE sessions SET version = ?1 WHERE stream_id = ?2 AND version = ?3",
                params![
                    integer(new_version)?,
                    stream.to_string(),
                    integer(expected.0)?
                ],
            )
            .map_err(storage)?;
        transaction.commit().map_err(storage)?;
        Ok(StreamVersion(new_version))
    }

    fn read(
        &self,
        stream: SessionId,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        let connection = self.reader()?;
        let mut statement = connection
            .prepare(
                "SELECT envelope_json FROM events
                 WHERE stream_id = ?1 AND sequence >= ?2
                 ORDER BY sequence
                 LIMIT ?3",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(
                params![
                    stream.to_string(),
                    integer(from_sequence)?,
                    limit_i64(limit)
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(storage)?;
        decode_rows(rows)
    }
}

fn configure(connection: &Connection, durability: Durability) -> Result<(), StoreError> {
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(storage)?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(storage)?;
    connection
        .pragma_update(None, "synchronous", durability.synchronous())
        .map_err(storage)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(storage)?;
    Ok(())
}

fn integer(value: u64) -> Result<i64, StoreError> {
    value
        .try_into()
        .map_err(|_| StoreError::Storage("value exceeds SQLite INTEGER".into()))
}

fn limit_i64(limit: usize) -> i64 {
    limit.try_into().unwrap_or(i64::MAX)
}

fn decode_rows(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<String>>,
) -> Result<Vec<EventEnvelope>, StoreError> {
    rows.map(|row| serde_json::from_str(&row.map_err(storage)?).map_err(serialization))
        .collect()
}

fn storage(error: rusqlite::Error) -> StoreError {
    StoreError::Storage(error.to_string())
}

fn serialization(error: serde_json::Error) -> StoreError {
    StoreError::Serialization(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{CorrelationId, EventId},
        event::{EventPayload, SchemaVersion},
    };
    use serde_json::Value;
    use std::{fs, sync::Arc, thread};
    use uuid::Uuid;

    fn event(stream: SessionId, sequence: u64) -> EventEnvelope {
        EventEnvelope::new(
            stream,
            sequence,
            Principal::System,
            None,
            CorrelationId::new(),
            SchemaVersion(1),
            "test.event",
            EventPayload::Inline { data: Value::Null },
        )
    }

    fn database_path() -> PathBuf {
        std::env::temp_dir().join(format!("arsy-sqlite-test-{}.db", Uuid::new_v4()))
    }

    fn remove_database(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn wal_store_batches_and_allows_concurrent_readers() {
        let path = database_path();
        let store = Arc::new(SqliteEventStore::open(&path, Durability::Strict).unwrap());
        let stream = SessionId::new();
        let batch: Vec<_> = (1..=25).map(|sequence| event(stream, sequence)).collect();
        assert_eq!(
            store.append(stream, StreamVersion(0), batch).unwrap(),
            StreamVersion(25)
        );
        assert_eq!(
            store.session_durability(stream).unwrap(),
            Some(Durability::Strict)
        );

        let mut readers = Vec::new();
        for _ in 0..4 {
            let store = Arc::clone(&store);
            readers.push(thread::spawn(move || {
                for _ in 0..50 {
                    assert!(!store.read(stream, 1, 100).unwrap().is_empty());
                }
            }));
        }
        for sequence in 26..=100 {
            store
                .append(
                    stream,
                    StreamVersion(sequence - 1),
                    vec![event(stream, sequence)],
                )
                .unwrap();
        }
        for reader in readers {
            reader.join().unwrap();
        }
        assert_eq!(store.read(stream, 1, 200).unwrap().len(), 100);
        assert_eq!(
            store.read_by_actor(&Principal::System, 200).unwrap().len(),
            100
        );
        assert!(matches!(
            store.append(stream, StreamVersion(99), vec![event(stream, 100)]),
            Err(StoreError::Conflict {
                actual: StreamVersion(100),
                ..
            })
        ));

        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        let indexes: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema
                 WHERE type = 'index' AND name IN ('events_actor_time', 'events_correlation')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexes, 2);
        drop(connection);
        drop(store);
        remove_database(&path);
    }

    #[test]
    fn duplicate_ids_are_rejected_by_the_unique_index() {
        let path = database_path();
        let store = SqliteEventStore::open(&path, Durability::Normal).unwrap();
        let stream = SessionId::new();
        let mut first = event(stream, 1);
        first.id = EventId::new();
        store
            .append(stream, StreamVersion(0), vec![first.clone()])
            .unwrap();
        let mut duplicate = event(stream, 2);
        duplicate.id = first.id;
        assert!(store
            .append(stream, StreamVersion(1), vec![duplicate])
            .is_err());
        assert_eq!(store.current_version(stream).unwrap(), StreamVersion(1));
        drop(store);
        remove_database(&path);
    }
}
