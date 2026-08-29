//! Canonical agent protocol: primitives, request envelope, version negotiation.
//!
//! Transport-neutral by design; see `docs/20-protocols.md`. Frontends speak this
//! protocol only, never runtime internals.

use crate::{
    domain::{
        AgentId, ApprovalId, ArtifactId, RequestId, ResourceRef, SessionId, StateVersion,
        SubscriptionId, TurnId, WorkspaceId,
    },
    event::EventEnvelope,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
};

/// Major version of the protocol implemented by this build.
pub const PROTOCOL_MAJOR: u32 = 1;
/// Minor version; minor features are additionally gated by capabilities.
pub const PROTOCOL_MINOR: u32 = 0;
/// Upper bound on an idempotency key, enforced before any ledger lookup.
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
/// Upper bound on events delivered in one subscription batch to a slow client.
pub const MAX_SUBSCRIPTION_BATCH: usize = 512;

/// Unknown optional fields carried forward across a minor version skew.
pub type Extensions = BTreeMap<String, Value>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ProtocolVersion {
    pub major: u32,
    pub minor: u32,
}

impl ProtocolVersion {
    pub const CURRENT: Self = Self {
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
    };

    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// Pick the highest client version sharing our major, capped at our minor.
    /// Majors never negotiate implicitly: a missing major is a hard failure.
    pub fn negotiate(offered: &[Self]) -> Result<Self, ProtocolError> {
        Self::CURRENT.negotiate_with(offered)
    }

    /// `negotiate` against an explicit local version; keeps the clamp testable
    /// while this build still sits at minor 0.
    pub fn negotiate_with(self, offered: &[Self]) -> Result<Self, ProtocolError> {
        offered
            .iter()
            .filter(|version| version.major == self.major)
            .map(|version| Self::new(self.major, version.minor.min(self.minor)))
            .max()
            .ok_or_else(|| ProtocolError::UnsupportedVersion {
                supported: self,
                offered: offered.to_vec(),
            })
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String")]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= MAX_IDEMPOTENCY_KEY_BYTES
            && value
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b'"');
        if valid {
            Ok(Self(value))
        } else {
            Err(ProtocolError::InvalidIdempotencyKey)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for IdempotencyKey {
    type Error = ProtocolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Request envelope. Unknown envelope fields are preserved in `extensions`
/// so a newer peer's optional additions survive a round trip.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProtocolEnvelope<T> {
    pub protocol: ProtocolVersion,
    pub request_id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
    pub payload: T,
    #[serde(default, flatten, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

impl<T> ProtocolEnvelope<T> {
    pub fn new(payload: T) -> Self {
        Self {
            protocol: ProtocolVersion::CURRENT,
            request_id: RequestId::new(),
            idempotency_key: None,
            payload,
            extensions: Extensions::new(),
        }
    }

    pub fn with_idempotency_key(mut self, key: IdempotencyKey) -> Self {
        self.idempotency_key = Some(key);
        self
    }
}

impl ProtocolEnvelope<ClientRequest> {
    /// Content digest of the retryable request: major version, method, and
    /// payload. `request_id` is excluded so a genuine retry matches, while a
    /// different body reusing the key is a conflict rather than a replay.
    pub fn request_digest(&self) -> Result<StateVersion, ProtocolError> {
        let canonical = serde_json::json!({
            "major": self.protocol.major,
            "request": &self.payload,
        });
        let bytes = serde_json::to_vec(&canonical)
            .map_err(|error| ProtocolError::Malformed(error.to_string()))?;
        Ok(StateVersion::from_digest(Sha256::digest(&bytes).into()))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum ClientRequest {
    Initialize(Initialize),
    WorkspaceOpen(WorkspaceOpen),
    SessionCreate(SessionCreate),
    SessionResume(SessionResume),
    TurnStart(TurnStart),
    ApprovalResolve(ApprovalResolution),
    AgentControl(AgentControl),
    ArtifactRead(ArtifactRead),
    Subscribe(Subscribe),
}

impl ClientRequest {
    /// Every accepted method name. Anything else fails as an unknown variant.
    pub const METHODS: [&'static str; 9] = [
        "initialize",
        "workspace_open",
        "session_create",
        "session_resume",
        "turn_start",
        "approval_resolve",
        "agent_control",
        "artifact_read",
        "subscribe",
    ];
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Initialize {
    pub protocol_versions: Vec<ProtocolVersion>,
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
    #[serde(default, flatten, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WorkspaceOpen {
    pub root: ResourceRef,
    #[serde(default, flatten, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionCreate {
    pub workspace: WorkspaceId,
    #[serde(default, flatten, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionResume {
    pub session: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_sequence: Option<u64>,
    #[serde(default, flatten, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TurnStart {
    pub session: SessionId,
    pub prompt: String,
    #[serde(default, flatten, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// Approval is security-relevant: it binds a decision to one operation digest,
/// so unknown fields are rejected instead of preserved.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalResolution {
    pub approval: ApprovalId,
    pub operation_digest: StateVersion,
    pub approved: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentControl {
    pub agent: AgentId,
    pub action: AgentAction,
    #[serde(default, flatten, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAction {
    Interrupt,
    Pause,
    Resume,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArtifactRead {
    pub artifact: ArtifactId,
    pub max_bytes: u64,
    #[serde(default, flatten, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Subscribe {
    pub session: SessionId,
    /// Resume point after a reconnect; absent means "from the beginning".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_sequence: Option<u64>,
    #[serde(default, flatten, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServerEvent {
    Initialized {
        protocol: ProtocolVersion,
        capabilities: BTreeSet<String>,
    },
    Accepted {
        request_id: RequestId,
        replay: bool,
    },
    Subscribed {
        subscription: SubscriptionId,
        next_sequence: u64,
    },
    /// One canonical fact delivered on a subscription.
    Stream {
        subscription: SubscriptionId,
        envelope: Box<EventEnvelope>,
    },
    /// A slow client was fast-forwarded; it must rebuild from `next_sequence`.
    Gap {
        subscription: SubscriptionId,
        dropped_from: u64,
        next_sequence: u64,
    },
    TurnCompleted {
        turn: TurnId,
    },
    Failed {
        request_id: RequestId,
        code: String,
        message: String,
    },
}

impl ServerEvent {
    pub const KINDS: [&'static str; 7] = [
        "initialized",
        "accepted",
        "subscribed",
        "stream",
        "gap",
        "turn_completed",
        "failed",
    ];
}

/// Decode a client request envelope, failing clearly on an unknown method.
pub fn decode_request(line: &str) -> Result<ProtocolEnvelope<ClientRequest>, ProtocolError> {
    decode(line, "method", &ClientRequest::METHODS)
}

/// Decode a server event envelope, failing clearly on an unknown event kind.
pub fn decode_event(line: &str) -> Result<ProtocolEnvelope<ServerEvent>, ProtocolError> {
    decode(line, "event", &ServerEvent::KINDS)
}

fn decode<T: serde::de::DeserializeOwned>(
    line: &str,
    tag: &str,
    known: &[&str],
) -> Result<ProtocolEnvelope<T>, ProtocolError> {
    let raw: Value =
        serde_json::from_str(line).map_err(|error| ProtocolError::Malformed(error.to_string()))?;
    let name = raw
        .get("payload")
        .and_then(|payload| payload.get(tag))
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::Malformed(format!("payload is missing `{tag}`")))?;
    if !known.contains(&name) {
        return Err(ProtocolError::UnknownVariant {
            tag: tag.to_owned(),
            name: name.to_owned(),
        });
    }
    serde_json::from_value(raw).map_err(|error| ProtocolError::Malformed(error.to_string()))
}

/// Verdict for a request carrying an idempotency key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    /// First sighting; the caller must execute it.
    Fresh,
    /// Same key and same body: a safe retry, do not execute again.
    Replay,
}

/// Bounded record of idempotency keys already admitted.
///
/// ponytail: in-memory and per-process; move behind the event store when the
/// agent service needs replay safety across restarts.
#[derive(Debug, Default)]
pub struct RequestLedger {
    seen: HashMap<IdempotencyKey, StateVersion>,
}

impl RequestLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests without a key are always `Fresh`; retries of a declared key are
    /// `Replay`, and the same key with a different body is a conflict.
    pub fn admit(
        &mut self,
        envelope: &ProtocolEnvelope<ClientRequest>,
    ) -> Result<Admission, ProtocolError> {
        let Some(key) = envelope.idempotency_key.clone() else {
            return Ok(Admission::Fresh);
        };
        let digest = envelope.request_digest()?;
        match self.seen.get(&key) {
            Some(previous) if *previous == digest => Ok(Admission::Replay),
            Some(_) => Err(ProtocolError::IdempotencyConflict {
                key: key.as_str().to_owned(),
            }),
            None => {
                self.seen.insert(key, digest);
                Ok(Admission::Fresh)
            }
        }
    }
}

/// Per-subscription delivery cursor; survives reconnects as `next_sequence`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionCursor {
    subscription: SubscriptionId,
    next_sequence: u64,
}

impl SubscriptionCursor {
    pub const fn resume(subscription: SubscriptionId, from_sequence: u64) -> Self {
        Self {
            subscription,
            next_sequence: from_sequence,
        }
    }

    pub const fn subscription(&self) -> SubscriptionId {
        self.subscription
    }

    /// Sequence the client must be served next after a reconnect.
    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Accept the next event in order; a hole is reported instead of skipped.
    pub fn accept(&mut self, envelope: &EventEnvelope) -> Result<(), ProtocolError> {
        if envelope.sequence != self.next_sequence {
            return Err(ProtocolError::SequenceGap {
                expected: self.next_sequence,
                actual: envelope.sequence,
            });
        }
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(ProtocolError::SequenceOverflow)?;
        Ok(())
    }

    /// Fast-forward a slow client, emitting the bounded gap it must reconcile.
    pub fn fast_forward(&mut self, to_sequence: u64) -> Option<ServerEvent> {
        if to_sequence <= self.next_sequence {
            return None;
        }
        let dropped_from = self.next_sequence;
        self.next_sequence = to_sequence;
        Some(ServerEvent::Gap {
            subscription: self.subscription,
            dropped_from,
            next_sequence: to_sequence,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProtocolError {
    UnsupportedVersion {
        supported: ProtocolVersion,
        offered: Vec<ProtocolVersion>,
    },
    UnknownVariant {
        tag: String,
        name: String,
    },
    Malformed(String),
    InvalidIdempotencyKey,
    IdempotencyConflict {
        key: String,
    },
    SequenceGap {
        expected: u64,
        actual: u64,
    },
    SequenceOverflow,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { supported, offered } => {
                let offered = offered
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    formatter,
                    "no shared protocol major version: this build speaks {supported}, peer offered [{offered}]"
                )
            }
            Self::UnknownVariant { tag, name } => {
                write!(formatter, "unknown protocol {tag} `{name}`")
            }
            Self::Malformed(detail) => write!(formatter, "malformed protocol message: {detail}"),
            Self::InvalidIdempotencyKey => write!(
                formatter,
                "idempotency key must be 1..={MAX_IDEMPOTENCY_KEY_BYTES} printable ASCII bytes"
            ),
            Self::IdempotencyConflict { key } => write!(
                formatter,
                "idempotency key `{key}` was already used for a different request"
            ),
            Self::SequenceGap { expected, actual } => write!(
                formatter,
                "subscription expected sequence {expected} but received {actual}"
            ),
            Self::SequenceOverflow => formatter.write_str("subscription sequence overflow"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{CorrelationId, Principal},
        event::{EventPayload, SchemaVersion},
    };

    fn turn_request(prompt: &str) -> ProtocolEnvelope<ClientRequest> {
        ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
            session: SessionId::new(),
            prompt: prompt.to_owned(),
            extensions: Extensions::new(),
        }))
    }

    fn event(sequence: u64) -> EventEnvelope {
        EventEnvelope::new(
            SessionId::new(),
            sequence,
            Principal::System,
            None,
            CorrelationId::new(),
            SchemaVersion(1),
            "turn.delta",
            EventPayload::Inline { data: Value::Null },
        )
    }

    #[test]
    fn major_versions_negotiate_and_minors_clamp() {
        assert_eq!(
            ProtocolVersion::negotiate(&[
                ProtocolVersion::new(1, 0),
                ProtocolVersion::new(1, 7),
                ProtocolVersion::new(2, 0),
            ])
            .unwrap(),
            ProtocolVersion::CURRENT
        );

        let local = ProtocolVersion::new(1, 3);
        assert_eq!(
            local
                .negotiate_with(&[ProtocolVersion::new(1, 1), ProtocolVersion::new(1, 9)])
                .unwrap(),
            local
        );
        assert_eq!(
            local
                .negotiate_with(&[ProtocolVersion::new(1, 1), ProtocolVersion::new(1, 2)])
                .unwrap(),
            ProtocolVersion::new(1, 2)
        );

        let error = ProtocolVersion::negotiate(&[ProtocolVersion::new(2, 3)]).unwrap_err();
        assert!(
            matches!(error, ProtocolError::UnsupportedVersion { .. }),
            "{error}"
        );
        assert!(ProtocolVersion::negotiate(&[]).is_err());
    }

    #[test]
    fn unknown_optional_fields_survive_a_round_trip() {
        let line = serde_json::json!({
            "protocol": { "major": 1, "minor": 0 },
            "request_id": SessionId::new().to_string(),
            "trace_parent": "00-abc",
            "payload": {
                "method": "turn_start",
                "params": {
                    "session": SessionId::new().to_string(),
                    "prompt": "hello",
                    "thinking_budget": 512
                }
            }
        })
        .to_string();

        let decoded = decode_request(&line).unwrap();
        assert_eq!(
            decoded.extensions.get("trace_parent"),
            Some(&Value::from("00-abc"))
        );
        let ClientRequest::TurnStart(turn) = &decoded.payload else {
            panic!("expected turn_start");
        };
        assert_eq!(
            turn.extensions.get("thinking_budget"),
            Some(&Value::from(512))
        );

        let reencoded = serde_json::to_string(&decoded).unwrap();
        assert_eq!(decode_request(&reencoded).unwrap(), decoded);
    }

    #[test]
    fn unknown_variants_and_unsafe_unknown_fields_fail_clearly() {
        let line = serde_json::json!({
            "protocol": { "major": 1, "minor": 0 },
            "request_id": SessionId::new().to_string(),
            "payload": { "method": "self_destruct", "params": {} }
        })
        .to_string();
        let error = decode_request(&line).unwrap_err();
        assert_eq!(
            error,
            ProtocolError::UnknownVariant {
                tag: "method".to_owned(),
                name: "self_destruct".to_owned(),
            }
        );
        assert_eq!(error.to_string(), "unknown protocol method `self_destruct`");

        let approval = serde_json::json!({
            "protocol": { "major": 1, "minor": 0 },
            "request_id": SessionId::new().to_string(),
            "payload": {
                "method": "approval_resolve",
                "params": {
                    "approval": SessionId::new().to_string(),
                    "operation_digest": "ab".repeat(32),
                    "approved": true,
                    "scope_override": "*"
                }
            }
        })
        .to_string();
        assert!(matches!(
            decode_request(&approval).unwrap_err(),
            ProtocolError::Malformed(_)
        ));
    }

    #[test]
    fn declared_idempotency_keys_are_safe_to_retry() {
        let key = IdempotencyKey::new("turn-1").unwrap();
        let request = turn_request("hello").with_idempotency_key(key.clone());
        let mut ledger = RequestLedger::new();

        assert_eq!(ledger.admit(&request).unwrap(), Admission::Fresh);
        // Same body retried under a fresh request id is a replay, not a re-run.
        let retry = ProtocolEnvelope {
            request_id: RequestId::new(),
            ..request.clone()
        };
        assert_eq!(ledger.admit(&retry).unwrap(), Admission::Replay);

        let conflicting = turn_request("different").with_idempotency_key(key);
        assert_eq!(
            ledger.admit(&conflicting).unwrap_err(),
            ProtocolError::IdempotencyConflict {
                key: "turn-1".to_owned()
            }
        );

        assert_eq!(
            ledger.admit(&turn_request("hello")).unwrap(),
            Admission::Fresh
        );
        assert_eq!(
            IdempotencyKey::new("").unwrap_err(),
            ProtocolError::InvalidIdempotencyKey
        );
        assert!(IdempotencyKey::new("x".repeat(MAX_IDEMPOTENCY_KEY_BYTES + 1)).is_err());
    }

    #[test]
    fn subscriptions_resume_from_a_cursor_and_report_gaps() {
        let subscription = SubscriptionId::new();
        let mut cursor = SubscriptionCursor::resume(subscription, 7);
        assert_eq!(cursor.next_sequence(), 7);

        assert_eq!(
            cursor.accept(&event(9)).unwrap_err(),
            ProtocolError::SequenceGap {
                expected: 7,
                actual: 9
            }
        );
        cursor.accept(&event(7)).unwrap();
        assert_eq!(cursor.next_sequence(), 8);

        let gap = cursor
            .fast_forward(8 + MAX_SUBSCRIPTION_BATCH as u64)
            .unwrap();
        assert_eq!(
            gap,
            ServerEvent::Gap {
                subscription,
                dropped_from: 8,
                next_sequence: 8 + MAX_SUBSCRIPTION_BATCH as u64,
            }
        );
        assert!(cursor.fast_forward(cursor.next_sequence()).is_none());

        let line = serde_json::to_string(&ProtocolEnvelope::new(gap)).unwrap();
        assert!(decode_event(&line).is_ok());
    }
}
