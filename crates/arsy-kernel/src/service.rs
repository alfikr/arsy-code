//! Embedded agent service: the single session coordinator.
//!
//! It is the only component permitted to append canonical events for a
//! session, so turn lifecycle, idempotency, and subscription delivery are all
//! decided in one place; see `docs/04-system-architecture.md`.

use crate::{
    domain::{CorrelationId, EventId, Principal, SessionId, StateVersion, SubscriptionId, TurnId},
    event::{EventEnvelope, EventPayload, EventStore, SchemaVersion, StoreError, StreamVersion},
    projection::{ProjectionError, ProjectionSet, TurnStatus},
    protocol::{
        ClientRequest, IdempotencyKey, ProtocolEnvelope, ProtocolError, RequestLedger, ServerEvent,
        SubscriptionCursor, MAX_SUBSCRIPTION_BATCH,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt,
    sync::{Arc, Mutex},
};

/// Schema version stamped on every event this service appends.
pub const SERVICE_SCHEMA: SchemaVersion = SchemaVersion(1);
/// Events buffered per subscriber before it is fast-forwarded with a gap.
pub const MAX_SUBSCRIBER_QUEUE: usize = MAX_SUBSCRIPTION_BATCH;

/// Evidence recorded with `turn.started`: what the client asked for, and under
/// which key, so a replay can be proved rather than assumed.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TurnStartedEvidence {
    pub turn_id: TurnId,
    pub prompt: String,
    pub request_digest: StateVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
}

/// Evidence recorded when a turn leaves the running state.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TurnFinishedEvidence {
    pub turn_id: TurnId,
    pub started_sequence: u64,
    pub outcome_digest: StateVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TurnFailure>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TurnFailure {
    pub code: String,
    pub message: String,
}

/// Verdict for a `turn_start` request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnAdmission {
    pub turn: TurnId,
    /// True when the idempotency key was already admitted: nothing was appended
    /// and the caller must not execute the turn again.
    pub replay: bool,
}

pub struct AgentService {
    store: Arc<dyn EventStore>,
    session: SessionId,
    state: Mutex<ServiceState>,
}

struct ServiceState {
    version: StreamVersion,
    projection: ProjectionSet,
    ledger: RequestLedger,
    turns_by_key: HashMap<IdempotencyKey, TurnId>,
    started_at: HashMap<TurnId, u64>,
    subscribers: BTreeMap<SubscriptionId, Subscriber>,
}

impl AgentService {
    /// Attach to a session, rebuilding lifecycle and idempotency state from the
    /// committed events. A crash mid-turn therefore resumes from the last
    /// committed event: unfinished turns stay visible via `unfinished_turns`.
    pub fn attach(store: Arc<dyn EventStore>, session: SessionId) -> Result<Self, ServiceError> {
        let mut projection = ProjectionSet::new(session);
        let mut ledger = RequestLedger::new();
        let mut turns_by_key = HashMap::new();
        let mut started_at = HashMap::new();
        let mut next = 1;

        loop {
            let page = store.read(session, next, MAX_SUBSCRIPTION_BATCH)?;
            if page.is_empty() {
                break;
            }
            for event in &page {
                projection.apply(event)?;
                if event.kind == TURN_STARTED {
                    let evidence: TurnStartedEvidence = inline(event)?;
                    started_at.insert(evidence.turn_id, event.sequence);
                    if let Some(key) = evidence.idempotency_key {
                        ledger.record(key.clone(), evidence.request_digest);
                        turns_by_key.insert(key, evidence.turn_id);
                    }
                }
                next = event
                    .sequence
                    .checked_add(1)
                    .ok_or(ServiceError::Store(StoreError::SequenceOverflow))?;
            }
        }

        Ok(Self {
            store,
            session,
            state: Mutex::new(ServiceState {
                version: projection.applied_version(),
                projection,
                ledger,
                turns_by_key,
                started_at,
                subscribers: BTreeMap::new(),
            }),
        })
    }

    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Admit a `turn_start` request and append `turn.started`.
    ///
    /// A repeated idempotency key with the same body returns the original turn
    /// without appending; the same key with a different body is a conflict.
    pub fn start_turn(
        &self,
        actor: Principal,
        envelope: &ProtocolEnvelope<ClientRequest>,
    ) -> Result<TurnAdmission, ServiceError> {
        let ClientRequest::TurnStart(request) = &envelope.payload else {
            return Err(ServiceError::UnexpectedMethod);
        };
        if request.session != self.session {
            return Err(ServiceError::WrongSession);
        }
        let digest = envelope.request_digest()?;
        let mut state = self.lock()?;

        if let Some(key) = &envelope.idempotency_key {
            // `admit` owns conflict detection; the turn map only recalls the outcome.
            if state.ledger.admit(envelope)?.is_replay() {
                let turn = *state
                    .turns_by_key
                    .get(key)
                    .ok_or(ServiceError::LedgerDesync)?;
                return Ok(TurnAdmission { turn, replay: true });
            }
        }

        let turn = TurnId::new();
        let evidence = TurnStartedEvidence {
            turn_id: turn,
            // ponytail: prompts above the inline event bound are rejected by the
            // store; move the body to the artifact CAS when P2 owns prompt bodies.
            prompt: request.prompt.clone(),
            request_digest: digest,
            idempotency_key: envelope.idempotency_key.clone(),
        };
        let sequence = self.append(&mut state, actor, TURN_STARTED, &evidence)?;
        state.started_at.insert(turn, sequence);
        if let Some(key) = envelope.idempotency_key.clone() {
            state.turns_by_key.insert(key, turn);
        }
        Ok(TurnAdmission {
            turn,
            replay: false,
        })
    }

    /// Append `turn.completed` with the digest of the turn outcome.
    pub fn complete_turn(
        &self,
        actor: Principal,
        turn: TurnId,
        outcome: &Value,
    ) -> Result<StreamVersion, ServiceError> {
        self.finish_turn(actor, turn, TURN_COMPLETED, outcome, None)
    }

    /// Append `turn.failed`; a resumed service uses this to close a turn that
    /// was still running when the process died.
    pub fn fail_turn(
        &self,
        actor: Principal,
        turn: TurnId,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<StreamVersion, ServiceError> {
        let failure = TurnFailure {
            code: code.into(),
            message: message.into(),
        };
        self.finish_turn(actor, turn, TURN_FAILED, &Value::Null, Some(failure))
    }

    fn finish_turn(
        &self,
        actor: Principal,
        turn: TurnId,
        kind: &str,
        outcome: &Value,
        failure: Option<TurnFailure>,
    ) -> Result<StreamVersion, ServiceError> {
        let mut state = self.lock()?;
        let running = state
            .projection
            .turns()
            .get(&turn)
            .ok_or(ServiceError::UnknownTurn(turn))?
            .status
            == TurnStatus::Running;
        if !running {
            return Err(ServiceError::TurnNotRunning(turn));
        }
        let started_sequence = *state
            .started_at
            .get(&turn)
            .ok_or(ServiceError::UnknownTurn(turn))?;
        let evidence = TurnFinishedEvidence {
            turn_id: turn,
            started_sequence,
            outcome_digest: digest_of(outcome)?,
            failure,
        };
        self.append(&mut state, actor, kind, &evidence)?;
        Ok(state.version)
    }

    /// Turns that were running at the last committed event.
    pub fn unfinished_turns(&self) -> Result<Vec<TurnId>, ServiceError> {
        let state = self.lock()?;
        Ok(state
            .projection
            .turns()
            .values()
            .filter(|turn| turn.status == TurnStatus::Running)
            .map(|turn| turn.id)
            .collect())
    }

    pub fn committed_version(&self) -> Result<StreamVersion, ServiceError> {
        Ok(self.lock()?.version)
    }

    /// Register a subscriber and hand back its bounded catch-up snapshot.
    ///
    /// At most `MAX_SUBSCRIPTION_BATCH` committed events are replayed; anything
    /// older than that is reported as a single gap the client must reconcile.
    pub fn subscribe(&self, from_sequence: u64) -> Result<Vec<ServerEvent>, ServiceError> {
        let mut state = self.lock()?;
        let snapshot = self
            .store
            .read(self.session, from_sequence, MAX_SUBSCRIPTION_BATCH)?;
        let subscription = SubscriptionId::new();
        let mut cursor = SubscriptionCursor::resume(subscription, from_sequence);
        let mut events = vec![ServerEvent::Subscribed {
            subscription,
            next_sequence: from_sequence,
        }];
        for envelope in snapshot {
            cursor.accept(&envelope)?;
            events.push(ServerEvent::Stream {
                subscription,
                envelope: Box::new(envelope),
            });
        }
        if let Some(head) = state.version.0.checked_add(1) {
            if let Some(gap) = cursor.fast_forward(head) {
                events.push(gap);
            }
        }
        state.subscribers.insert(
            subscription,
            Subscriber {
                cursor,
                pending: VecDeque::new(),
                gap: None,
            },
        );
        Ok(events)
    }

    /// Drain everything buffered for a subscriber since its last poll.
    pub fn poll(&self, subscription: SubscriptionId) -> Result<Vec<ServerEvent>, ServiceError> {
        let mut state = self.lock()?;
        state
            .subscribers
            .get_mut(&subscription)
            .ok_or(ServiceError::UnknownSubscription(subscription))?
            .drain()
    }

    pub fn unsubscribe(&self, subscription: SubscriptionId) -> Result<(), ServiceError> {
        self.lock()?.subscribers.remove(&subscription);
        Ok(())
    }

    /// Append one event, then fan it out. Fan-out only touches per-subscriber
    /// buffers, so a slow subscriber can never block or fail a commit.
    fn append<T: Serialize>(
        &self,
        state: &mut ServiceState,
        actor: Principal,
        kind: &str,
        payload: &T,
    ) -> Result<u64, ServiceError> {
        let sequence = state
            .version
            .0
            .checked_add(1)
            .ok_or(ServiceError::Store(StoreError::SequenceOverflow))?;
        let data = serde_json::to_value(payload)
            .map_err(|error| ServiceError::Store(StoreError::Serialization(error.to_string())))?;
        let envelope = EventEnvelope::new(
            self.session,
            sequence,
            actor,
            state.last_event_id(),
            CorrelationId::new(),
            SERVICE_SCHEMA,
            kind,
            EventPayload::Inline { data },
        );
        let version = self
            .store
            .append(self.session, state.version, vec![envelope.clone()])?;
        state.projection.apply(&envelope)?;
        state.version = version;
        for subscriber in state.subscribers.values_mut() {
            subscriber.offer(&envelope);
        }
        Ok(sequence)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, ServiceState>, ServiceError> {
        self.state.lock().map_err(|_| ServiceError::Poisoned)
    }
}

impl ServiceState {
    fn last_event_id(&self) -> Option<EventId> {
        self.projection.audit().last().map(|entry| entry.event_id)
    }
}

/// One subscriber's bounded buffer. Overflow collapses into a single gap so
/// memory stays bounded regardless of how far behind the client falls.
struct Subscriber {
    cursor: SubscriptionCursor,
    pending: VecDeque<EventEnvelope>,
    gap: Option<ServerEvent>,
}

impl Subscriber {
    fn offer(&mut self, envelope: &EventEnvelope) {
        let expected = self
            .cursor
            .next_sequence()
            .saturating_add(self.pending.len() as u64);
        if envelope.sequence != expected {
            return;
        }
        if self.pending.len() >= MAX_SUBSCRIBER_QUEUE {
            self.pending.clear();
            if let Some(next) = envelope.sequence.checked_add(1) {
                // Repeated overflows coalesce into one gap that still starts at
                // the oldest sequence the client never saw.
                let oldest = match self.gap.take() {
                    Some(ServerEvent::Gap { dropped_from, .. }) => Some(dropped_from),
                    _ => None,
                };
                self.gap = self
                    .cursor
                    .fast_forward(next)
                    .map(|gap| match (oldest, gap) {
                        (
                            Some(dropped_from),
                            ServerEvent::Gap {
                                subscription,
                                next_sequence,
                                ..
                            },
                        ) => ServerEvent::Gap {
                            subscription,
                            dropped_from,
                            next_sequence,
                        },
                        (_, gap) => gap,
                    });
            }
            return;
        }
        self.pending.push_back(envelope.clone());
    }

    fn drain(&mut self) -> Result<Vec<ServerEvent>, ServiceError> {
        let mut events = Vec::with_capacity(self.pending.len() + 1);
        if let Some(gap) = self.gap.take() {
            events.push(gap);
        }
        let subscription = self.cursor.subscription();
        for envelope in std::mem::take(&mut self.pending) {
            self.cursor.accept(&envelope)?;
            events.push(ServerEvent::Stream {
                subscription,
                envelope: Box::new(envelope),
            });
        }
        Ok(events)
    }
}

const TURN_STARTED: &str = "turn.started";
const TURN_COMPLETED: &str = "turn.completed";
const TURN_FAILED: &str = "turn.failed";

fn digest_of(value: &Value) -> Result<StateVersion, ServiceError> {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(value)
        .map_err(|error| ServiceError::Store(StoreError::Serialization(error.to_string())))?;
    Ok(StateVersion::from_digest(Sha256::digest(&bytes).into()))
}

fn inline<T: serde::de::DeserializeOwned>(event: &EventEnvelope) -> Result<T, ServiceError> {
    let EventPayload::Inline { data } = &event.payload else {
        return Err(ServiceError::MissingEvidence(event.kind.clone()));
    };
    serde_json::from_value(data.clone())
        .map_err(|_| ServiceError::MissingEvidence(event.kind.clone()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceError {
    Store(StoreError),
    Projection(ProjectionError),
    Protocol(ProtocolError),
    UnexpectedMethod,
    WrongSession,
    UnknownTurn(TurnId),
    TurnNotRunning(TurnId),
    UnknownSubscription(SubscriptionId),
    MissingEvidence(String),
    LedgerDesync,
    Poisoned,
}

impl From<StoreError> for ServiceError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<ProjectionError> for ServiceError {
    fn from(value: ProjectionError) -> Self {
        Self::Projection(value)
    }
}

impl From<ProtocolError> for ServiceError {
    fn from(value: ProtocolError) -> Self {
        Self::Protocol(value)
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "event store: {error}"),
            Self::Projection(error) => write!(formatter, "projection: {error}"),
            Self::Protocol(error) => write!(formatter, "protocol: {error}"),
            Self::UnexpectedMethod => formatter.write_str("request is not a turn_start"),
            Self::WrongSession => formatter.write_str("request targets another session"),
            Self::UnknownTurn(turn) => write!(formatter, "turn {turn} does not exist"),
            Self::TurnNotRunning(turn) => write!(formatter, "turn {turn} is already finished"),
            Self::UnknownSubscription(id) => write!(formatter, "subscription {id} does not exist"),
            Self::MissingEvidence(kind) => {
                write!(formatter, "{kind} is missing its inline evidence")
            }
            Self::LedgerDesync => {
                formatter.write_str("idempotency key was admitted without a recorded turn")
            }
            Self::Poisoned => formatter.write_str("agent service lock poisoned"),
        }
    }
}

impl std::error::Error for ServiceError {}
