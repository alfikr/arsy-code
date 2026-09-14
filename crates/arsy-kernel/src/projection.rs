use crate::{
    domain::{CorrelationId, EventId, Principal, SessionId, TurnId},
    event::{EventEnvelope, EventPayload, StreamVersion},
};
use serde::Deserialize;
use std::{collections::BTreeMap, fmt};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnView {
    pub id: TurnId,
    pub started_sequence: u64,
    pub finished_sequence: Option<u64>,
    pub status: TurnStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditEntry {
    pub sequence: u64,
    pub event_id: EventId,
    pub occurred_at_ms: u64,
    pub actor: Principal,
    pub causation: Option<EventId>,
    pub correlation: CorrelationId,
    pub kind: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// What it cost in micros, or `None` when nothing said what the model
    /// charges.
    ///
    /// An `Option` rather than a zero because the two are different facts and
    /// only one of them is free. Summed across a session, one unpriced turn
    /// makes the total unknown: a figure that quietly omits some of the spend
    /// is worse than an honest refusal to name one, because only the second
    /// tells an operator to go and configure the price.
    pub cost_micros: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionSet {
    session: SessionId,
    applied: StreamVersion,
    turns: BTreeMap<TurnId, TurnView>,
    audit: Vec<AuditEntry>,
    usage: UsageTotals,
}

impl ProjectionSet {
    pub fn new(session: SessionId) -> Self {
        Self {
            session,
            applied: StreamVersion(0),
            turns: BTreeMap::new(),
            audit: Vec::new(),
            // A session that has spent nothing has cost nothing, and knows it.
            // Only a turn that was actually run without a price makes the
            // running total unknown.
            usage: UsageTotals {
                cost_micros: Some(0),
                ..UsageTotals::default()
            },
        }
    }

    pub fn rebuild<'a>(
        session: SessionId,
        events: impl IntoIterator<Item = &'a EventEnvelope>,
    ) -> Result<Self, ProjectionError> {
        let mut projection = Self::new(session);
        for event in events {
            projection.apply(event)?;
        }
        Ok(projection)
    }

    pub fn apply(&mut self, event: &EventEnvelope) -> Result<(), ProjectionError> {
        if event.session != self.session {
            return Err(ProjectionError::StreamMismatch);
        }
        let expected = self
            .applied
            .0
            .checked_add(1)
            .ok_or(ProjectionError::Overflow)?;
        if event.sequence != expected {
            return Err(ProjectionError::SequenceGap {
                expected,
                actual: event.sequence,
            });
        }

        let delta = ProjectionDelta::from_event(event, &self.turns)?;
        match delta {
            ProjectionDelta::None => {}
            ProjectionDelta::Start(turn) => {
                self.turns.insert(turn.id, turn);
            }
            ProjectionDelta::Finish { id, status } => {
                let turn = self
                    .turns
                    .get_mut(&id)
                    .ok_or(ProjectionError::UnknownTurn(id))?;
                turn.status = status;
                turn.finished_sequence = Some(event.sequence);
            }
            ProjectionDelta::Usage(usage) => {
                let input_tokens = self
                    .usage
                    .input_tokens
                    .checked_add(usage.input_tokens)
                    .ok_or(ProjectionError::Overflow)?;
                let output_tokens = self
                    .usage
                    .output_tokens
                    .checked_add(usage.output_tokens)
                    .ok_or(ProjectionError::Overflow)?;
                let cost_micros = match (self.usage.cost_micros, usage.cost_micros) {
                    (Some(total), Some(next)) => {
                        Some(total.checked_add(next).ok_or(ProjectionError::Overflow)?)
                    }
                    _ => None,
                };
                self.usage = UsageTotals {
                    input_tokens,
                    output_tokens,
                    cost_micros,
                };
            }
        }
        self.audit.push(AuditEntry {
            sequence: event.sequence,
            event_id: event.id,
            occurred_at_ms: event.occurred_at_ms,
            actor: event.actor.clone(),
            causation: event.causation,
            correlation: event.correlation,
            kind: event.kind.clone(),
        });
        self.applied = StreamVersion(event.sequence);
        Ok(())
    }

    pub fn turns(&self) -> &BTreeMap<TurnId, TurnView> {
        &self.turns
    }

    pub fn audit(&self) -> &[AuditEntry] {
        &self.audit
    }

    pub const fn usage(&self) -> UsageTotals {
        self.usage
    }

    pub const fn applied_version(&self) -> StreamVersion {
        self.applied
    }

    pub const fn lag(&self, source: StreamVersion) -> u64 {
        source.0.saturating_sub(self.applied.0)
    }
}

enum ProjectionDelta {
    None,
    Start(TurnView),
    Finish { id: TurnId, status: TurnStatus },
    Usage(UsageTotals),
}

impl ProjectionDelta {
    fn from_event(
        event: &EventEnvelope,
        turns: &BTreeMap<TurnId, TurnView>,
    ) -> Result<Self, ProjectionError> {
        match event.kind.as_str() {
            "turn.started" => {
                let payload: TurnPayload = inline(event)?;
                if turns.contains_key(&payload.turn_id) {
                    return Err(ProjectionError::DuplicateTurn(payload.turn_id));
                }
                Ok(Self::Start(TurnView {
                    id: payload.turn_id,
                    started_sequence: event.sequence,
                    finished_sequence: None,
                    status: TurnStatus::Running,
                }))
            }
            "turn.completed" | "turn.failed" => {
                let payload: TurnPayload = inline(event)?;
                if !turns.contains_key(&payload.turn_id) {
                    return Err(ProjectionError::UnknownTurn(payload.turn_id));
                }
                Ok(Self::Finish {
                    id: payload.turn_id,
                    status: if event.kind == "turn.completed" {
                        TurnStatus::Completed
                    } else {
                        TurnStatus::Failed
                    },
                })
            }
            "usage.recorded" => {
                let payload: UsagePayload = inline(event)?;
                Ok(Self::Usage(payload.into()))
            }
            _ => Ok(Self::None),
        }
    }
}

#[derive(Deserialize)]
struct TurnPayload {
    turn_id: TurnId,
}

impl From<UsagePayload> for UsageTotals {
    fn from(value: UsagePayload) -> Self {
        Self {
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            cost_micros: value.cost_micros,
        }
    }
}

#[derive(Deserialize)]
struct UsagePayload {
    input_tokens: u64,
    output_tokens: u64,
    /// Absent in a stream written before costs were recorded, which reads as
    /// unknown — which is exactly what it was.
    #[serde(default)]
    cost_micros: Option<u64>,
}

fn inline<T>(event: &EventEnvelope) -> Result<T, ProjectionError>
where
    T: serde::de::DeserializeOwned,
{
    let EventPayload::Inline { data } = &event.payload else {
        return Err(ProjectionError::InlinePayloadRequired(event.kind.clone()));
    };
    serde_json::from_value(data.clone()).map_err(|error| ProjectionError::InvalidPayload {
        kind: event.kind.clone(),
        message: error.to_string(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectionError {
    StreamMismatch,
    SequenceGap { expected: u64, actual: u64 },
    DuplicateTurn(TurnId),
    UnknownTurn(TurnId),
    InlinePayloadRequired(String),
    InvalidPayload { kind: String, message: String },
    Overflow,
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StreamMismatch => {
                formatter.write_str("projection event belongs to another stream")
            }
            Self::SequenceGap { expected, actual } => {
                write!(
                    formatter,
                    "projection expected sequence {expected}, got {actual}"
                )
            }
            Self::DuplicateTurn(id) => write!(formatter, "turn {id} already exists"),
            Self::UnknownTurn(id) => write!(formatter, "turn {id} does not exist"),
            Self::InlinePayloadRequired(kind) => {
                write!(formatter, "{kind} requires an inline projection payload")
            }
            Self::InvalidPayload { kind, message } => {
                write!(formatter, "invalid {kind} payload: {message}")
            }
            Self::Overflow => formatter.write_str("projection counter overflow"),
        }
    }
}

impl std::error::Error for ProjectionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::CorrelationId,
        event::{EventPayload, SchemaVersion},
    };
    use serde_json::json;

    fn event(
        session: SessionId,
        sequence: u64,
        kind: &str,
        data: serde_json::Value,
    ) -> EventEnvelope {
        EventEnvelope::new(
            session,
            sequence,
            Principal::System,
            None,
            CorrelationId::new(),
            SchemaVersion(1),
            kind,
            EventPayload::Inline { data },
        )
    }

    #[test]
    fn incremental_and_rebuild_are_identical_and_lag_is_visible() {
        let session = SessionId::new();
        let turn = TurnId::new();
        let events = vec![
            event(session, 1, "turn.started", json!({ "turn_id": turn })),
            event(
                session,
                2,
                "usage.recorded",
                json!({ "input_tokens": 10, "output_tokens": 4, "cost_micros": 25 }),
            ),
            event(session, 3, "turn.completed", json!({ "turn_id": turn })),
        ];

        let rebuilt = ProjectionSet::rebuild(session, &events).unwrap();
        let mut incremental = ProjectionSet::new(session);
        incremental.apply(&events[0]).unwrap();
        incremental.apply(&events[1]).unwrap();
        incremental.apply(&events[2]).unwrap();

        assert_eq!(incremental, rebuilt);
        assert_eq!(rebuilt.audit().len(), 3);
        assert_eq!(
            rebuilt.turns().get(&turn).unwrap().status,
            TurnStatus::Completed
        );
        assert_eq!(
            rebuilt.usage(),
            UsageTotals {
                input_tokens: 10,
                output_tokens: 4,
                cost_micros: Some(25),
            }
        );
        assert_eq!(rebuilt.lag(StreamVersion(5)), 2);
        assert_eq!(
            ProjectionSet::rebuild(session, &events).unwrap(),
            ProjectionSet::rebuild(session, &events).unwrap()
        );
    }

    #[test]
    fn rejected_event_does_not_mutate_projection() {
        let session = SessionId::new();
        let mut projection = ProjectionSet::new(session);
        let gap = event(session, 2, "unknown", serde_json::Value::Null);
        assert_eq!(
            projection.apply(&gap),
            Err(ProjectionError::SequenceGap {
                expected: 1,
                actual: 2
            })
        );
        assert_eq!(projection.applied_version(), StreamVersion(0));
        assert!(projection.audit().is_empty());
    }
}
