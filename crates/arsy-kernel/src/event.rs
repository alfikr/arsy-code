use crate::domain::{CorrelationId, EventId, Principal, ResourceRef, SessionId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::RwLock,
    time::{SystemTime, UNIX_EPOCH},
};

pub const MAX_INLINE_EVENT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct StreamVersion(pub u64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SchemaVersion(pub u32);

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "storage", rename_all = "snake_case")]
pub enum EventPayload {
    Inline {
        data: Value,
    },
    Artifact {
        reference: ResourceRef,
        media_type: String,
        size: u64,
    },
}

impl EventPayload {
    fn validate(&self) -> Result<(), StoreError> {
        if let Self::Inline { data } = self {
            let bytes = serde_json::to_vec(data)
                .map_err(|error| StoreError::Serialization(error.to_string()))?
                .len();
            if bytes > MAX_INLINE_EVENT_BYTES {
                return Err(StoreError::PayloadTooLarge {
                    bytes,
                    max: MAX_INLINE_EVENT_BYTES,
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EventEnvelope {
    pub id: EventId,
    pub session: SessionId,
    pub sequence: u64,
    pub occurred_at_ms: u64,
    pub actor: Principal,
    pub causation: Option<EventId>,
    pub correlation: CorrelationId,
    pub schema: SchemaVersion,
    pub kind: String,
    pub payload: EventPayload,
}

impl EventEnvelope {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: SessionId,
        sequence: u64,
        actor: Principal,
        causation: Option<EventId>,
        correlation: CorrelationId,
        schema: SchemaVersion,
        kind: impl Into<String>,
        payload: EventPayload,
    ) -> Self {
        let occurred_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        Self {
            id: EventId::new(),
            session,
            sequence,
            occurred_at_ms,
            actor,
            causation,
            correlation,
            schema,
            kind: kind.into(),
            payload,
        }
    }

    pub(crate) fn validate_for_append(
        &self,
        stream: SessionId,
        sequence: u64,
    ) -> Result<(), StoreError> {
        if self.session != stream {
            return Err(StoreError::StreamMismatch);
        }
        if self.sequence != sequence {
            return Err(StoreError::InvalidSequence {
                expected: sequence,
                actual: self.sequence,
            });
        }
        self.payload.validate()
    }
}

pub trait EventStore: Send + Sync {
    fn current_version(&self, stream: SessionId) -> Result<StreamVersion, StoreError>;

    fn append(
        &self,
        stream: SessionId,
        expected: StreamVersion,
        events: Vec<EventEnvelope>,
    ) -> Result<StreamVersion, StoreError>;

    fn read(
        &self,
        stream: SessionId,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, StoreError>;
}

#[derive(Default)]
pub struct MemoryEventStore {
    streams: RwLock<HashMap<SessionId, Vec<EventEnvelope>>>,
}

impl EventStore for MemoryEventStore {
    fn current_version(&self, stream: SessionId) -> Result<StreamVersion, StoreError> {
        let streams = self
            .streams
            .read()
            .map_err(|_| StoreError::Storage("event store lock poisoned".into()))?;
        Ok(StreamVersion(
            streams
                .get(&stream)
                .and_then(|events| events.last())
                .map_or(0, |event| event.sequence),
        ))
    }

    fn append(
        &self,
        stream: SessionId,
        expected: StreamVersion,
        events: Vec<EventEnvelope>,
    ) -> Result<StreamVersion, StoreError> {
        let mut streams = self
            .streams
            .write()
            .map_err(|_| StoreError::Storage("event store lock poisoned".into()))?;
        let stored = streams.entry(stream).or_default();
        let actual = StreamVersion(stored.last().map_or(0, |event| event.sequence));
        if actual != expected {
            return Err(StoreError::Conflict { expected, actual });
        }

        let mut ids: HashSet<_> = stored.iter().map(|event| event.id).collect();
        for (offset, event) in events.iter().enumerate() {
            let sequence = expected
                .0
                .checked_add(offset as u64 + 1)
                .ok_or(StoreError::SequenceOverflow)?;
            event.validate_for_append(stream, sequence)?;
            if !ids.insert(event.id) {
                return Err(StoreError::DuplicateEvent(event.id));
            }
        }

        stored.extend(events);
        Ok(StreamVersion(
            stored.last().map_or(expected.0, |event| event.sequence),
        ))
    }

    fn read(
        &self,
        stream: SessionId,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        let streams = self
            .streams
            .read()
            .map_err(|_| StoreError::Storage("event store lock poisoned".into()))?;
        Ok(streams
            .get(&stream)
            .into_iter()
            .flatten()
            .filter(|event| event.sequence >= from_sequence)
            .take(limit)
            .cloned()
            .collect())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreError {
    Conflict {
        expected: StreamVersion,
        actual: StreamVersion,
    },
    StreamMismatch,
    InvalidSequence {
        expected: u64,
        actual: u64,
    },
    SequenceOverflow,
    DuplicateEvent(EventId),
    PayloadTooLarge {
        bytes: usize,
        max: usize,
    },
    MigrationRequired {
        current: u32,
        expected: u32,
    },
    SchemaTooNew {
        current: u32,
        supported: u32,
    },
    Serialization(String),
    Storage(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { expected, actual } => write!(
                formatter,
                "stream version conflict: expected {}, actual {}",
                expected.0, actual.0
            ),
            Self::StreamMismatch => formatter.write_str("event belongs to a different stream"),
            Self::InvalidSequence { expected, actual } => {
                write!(
                    formatter,
                    "invalid event sequence: expected {expected}, actual {actual}"
                )
            }
            Self::SequenceOverflow => formatter.write_str("event sequence overflow"),
            Self::DuplicateEvent(id) => write!(formatter, "duplicate event ID {id}"),
            Self::PayloadTooLarge { bytes, max } => {
                write!(
                    formatter,
                    "inline payload is {bytes} bytes; maximum is {max}"
                )
            }
            Self::MigrationRequired { current, expected } => write!(
                formatter,
                "store schema is version {current}; this build needs {expected}, run a migration"
            ),
            Self::SchemaTooNew { current, supported } => write!(
                formatter,
                "store schema is version {current}; this build supports only {supported}"
            ),
            Self::Serialization(message) | Self::Storage(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for StoreError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(stream: SessionId, sequence: u64, payload: EventPayload) -> EventEnvelope {
        EventEnvelope::new(
            stream,
            sequence,
            Principal::System,
            None,
            CorrelationId::new(),
            SchemaVersion(1),
            "test.event",
            payload,
        )
    }

    #[test]
    fn optimistic_append_rejects_stale_and_oversized_events() {
        let store = MemoryEventStore::default();
        let stream = SessionId::new();
        let first = event(stream, 1, EventPayload::Inline { data: Value::Null });
        assert_eq!(
            store.append(stream, StreamVersion(0), vec![first]).unwrap(),
            StreamVersion(1)
        );

        let stale = event(stream, 1, EventPayload::Inline { data: Value::Null });
        assert_eq!(
            store.append(stream, StreamVersion(0), vec![stale]),
            Err(StoreError::Conflict {
                expected: StreamVersion(0),
                actual: StreamVersion(1)
            })
        );

        let second = event(stream, 2, EventPayload::Inline { data: Value::Null });
        store
            .append(stream, StreamVersion(1), vec![second.clone()])
            .unwrap();
        assert_eq!(store.read(stream, 2, 1).unwrap(), vec![second]);

        let oversized = event(
            stream,
            3,
            EventPayload::Inline {
                data: Value::String("x".repeat(MAX_INLINE_EVENT_BYTES)),
            },
        );
        assert!(matches!(
            store.append(stream, StreamVersion(2), vec![oversized]),
            Err(StoreError::PayloadTooLarge { .. })
        ));

        let artifact = event(
            stream,
            3,
            EventPayload::Artifact {
                reference: ResourceRef::new("artifact", "sha256/example").unwrap(),
                media_type: "application/octet-stream".into(),
                size: (MAX_INLINE_EVENT_BYTES + 1) as u64,
            },
        );
        assert_eq!(
            store
                .append(stream, StreamVersion(2), vec![artifact])
                .unwrap(),
            StreamVersion(3)
        );
    }
}
