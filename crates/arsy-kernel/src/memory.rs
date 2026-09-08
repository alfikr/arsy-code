//! Memory records: scoped claims with provenance, supersession, and revocation.
//!
//! See `docs/17-memory-knowledge.md`. Memory is where an old guess turns into a
//! durable prompt injection unless three rules hold, so they are enforced here
//! rather than left to whoever writes a record:
//!
//! * **Repository text cannot create trusted memory.** A record's origin is the
//!   authority of whatever wrote it. An untrusted origin cannot claim a
//!   confidence above `Reported`, and cannot supersede or revoke a record a
//!   more authoritative origin wrote.
//! * **Nothing is averaged away.** Retrieval returns conflicting live records
//!   together; reconciling them is a decision, not an aggregation.
//! * **A deletion is a tombstone.** Revoking removes a record from retrieval
//!   and leaves the audit trail, because "this was believed and then withdrawn"
//!   is the fact an audit needs.

use crate::{
    capability::PolicySource,
    domain::{ArtifactId, CorrelationId, MemoryId, Principal, SessionId},
    event::{EventEnvelope, EventPayload, EventStore, SchemaVersion, StoreError, StreamVersion},
    secret::Redactor,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, sync::Arc};

/// Where a memory applies. A record is only ever retrieved for the scope it was
/// written in: a repository fact never becomes a user fact on its own.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// The current unit of work; dropped when it ends.
    Working,
    Session(String),
    Task(String),
    Repository,
    Branch(String),
    User,
    Team(String),
    /// Learned heuristics are a separate class, kept out of the scopes above so
    /// an experiment cannot masquerade as an observed fact.
    Heuristic,
}

impl MemoryScope {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Session(_) => "session",
            Self::Task(_) => "task",
            Self::Repository => "repository",
            Self::Branch(_) => "branch",
            Self::User => "user",
            Self::Team(_) => "team",
            Self::Heuristic => "heuristic",
        }
    }
}

impl fmt::Display for MemoryScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(id) | Self::Task(id) | Self::Branch(id) | Self::Team(id) => {
                write!(formatter, "{}:{id}", self.kind())
            }
            other => formatter.write_str(other.kind()),
        }
    }
}

/// How much weight a claim carries. Ordered, so a comparison is meaningful.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Someone said it. The ceiling for anything an untrusted origin writes.
    Reported,
    /// Something checked it once.
    Observed,
    /// A check that can be repeated confirmed it.
    Verified,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    /// Replaced by another record, which names it.
    Superseded,
    /// Withdrawn. The record stays for audit and leaves retrieval.
    Revoked,
}

/// One thing believed, and why.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemoryRecord {
    pub id: MemoryId,
    pub scope: MemoryScope,
    /// The claim itself, stored in the artifact CAS rather than inline: a
    /// memory is evidence, and evidence is addressable.
    pub claim: ArtifactId,
    /// What the claim rests on. A record with none is `Reported` at best.
    pub provenance: Vec<ArtifactId>,
    pub confidence: Confidence,
    /// The authority that wrote it.
    pub origin: PolicySource,
    pub author: Principal,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub expires_at_ms: Option<u64>,
    pub status: MemoryStatus,
    /// Set when this record replaced another one.
    pub supersedes: Option<MemoryId>,
    /// Set when another record replaced this one.
    pub superseded_by: Option<MemoryId>,
    /// Why it was revoked, when it was.
    pub revocation: Option<String>,
}

impl MemoryRecord {
    /// Whether this record should be returned to a caller now.
    pub fn is_live(&self, now_ms: u64) -> bool {
        self.status == MemoryStatus::Active
            && self.expires_at_ms.is_none_or(|expiry| now_ms < expiry)
    }
}

/// A record as a caller proposes it, before the store decides whether it may
/// exist.
#[derive(Clone, Debug, PartialEq)]
pub struct NewMemory {
    pub scope: MemoryScope,
    pub claim: ArtifactId,
    pub provenance: Vec<ArtifactId>,
    pub confidence: Confidence,
    pub origin: PolicySource,
    pub author: Principal,
    pub expires_at_ms: Option<u64>,
    /// The record this one replaces, if any.
    pub supersedes: Option<MemoryId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryError {
    Unknown(MemoryId),
    /// The record named is not one this origin may act on.
    NotPermitted {
        origin: PolicySource,
        reason: String,
    },
    /// A claim that looks like a credential is never stored here.
    SecretLike(String),
    /// Superseding a record in another scope would move a fact between scopes.
    ScopeMismatch,
    /// The record is already superseded or revoked.
    NotActive(MemoryId),
    /// The index accepted the change and the store did not.
    Storage(String),
}

impl fmt::Display for MemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(id) => write!(formatter, "no memory record {id}"),
            Self::NotPermitted { origin, reason } => {
                write!(formatter, "a {origin} record {reason}")
            }
            Self::SecretLike(detail) => write!(
                formatter,
                "the claim looks like a credential ({detail}); store it with `arsy auth set`"
            ),
            Self::ScopeMismatch => {
                formatter.write_str("a record may only supersede one in its own scope")
            }
            Self::NotActive(id) => write!(formatter, "memory record {id} is not active"),
            Self::Storage(error) => write!(formatter, "memory could not be recorded: {error}"),
        }
    }
}

impl std::error::Error for MemoryError {}

/// The records one workspace holds.
///
/// In memory: the durable form is the artifact CAS the claims live in plus the
/// event log that records the writes, and a rebuilt index is cheaper than a
/// second source of truth that can disagree with it.
#[derive(Debug, Default)]
pub struct MemoryIndex {
    records: BTreeMap<MemoryId, MemoryRecord>,
}

impl MemoryIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild from records read back in any order.
    pub fn rebuild(records: impl IntoIterator<Item = MemoryRecord>) -> Self {
        Self {
            records: records
                .into_iter()
                .map(|record| (record.id, record))
                .collect(),
        }
    }

    pub fn get(&self, id: MemoryId) -> Option<&MemoryRecord> {
        self.records.get(&id)
    }

    /// Every record, live or not. Audit reads this; retrieval does not.
    pub fn all(&self) -> impl Iterator<Item = &MemoryRecord> {
        self.records.values()
    }

    /// Record a claim.
    ///
    /// `claim_text` is the claim as it will be read back, checked here because
    /// a credential that reaches memory is a credential in every later prompt.
    pub fn remember(
        &mut self,
        new: NewMemory,
        claim_text: &str,
        redactor: &Redactor,
        now_ms: u64,
    ) -> Result<MemoryId, MemoryError> {
        reject_secret_like(claim_text, redactor)?;
        // Repository and session content may state things; it may not certify
        // them. Capping rather than rejecting keeps the claim, which is often
        // useful, without letting it outrank an operator's.
        let confidence = if new.origin.may_grant() {
            new.confidence
        } else {
            new.confidence.min(Confidence::Reported)
        };
        // A claim resting on nothing is at most reported, whoever wrote it.
        let confidence = if new.provenance.is_empty() {
            confidence.min(Confidence::Reported)
        } else {
            confidence
        };

        if let Some(previous) = new.supersedes {
            let existing = self
                .records
                .get(&previous)
                .ok_or(MemoryError::Unknown(previous))?;
            if existing.scope != new.scope {
                return Err(MemoryError::ScopeMismatch);
            }
            self.check_authority(existing, new.origin, "cannot supersede")?;
            if existing.status != MemoryStatus::Active {
                return Err(MemoryError::NotActive(previous));
            }
        }

        let id = MemoryId::new();
        if let Some(previous) = new.supersedes {
            let existing = self.records.get_mut(&previous).expect("checked above");
            existing.status = MemoryStatus::Superseded;
            existing.superseded_by = Some(id);
            existing.updated_at_ms = now_ms;
        }
        self.records.insert(
            id,
            MemoryRecord {
                id,
                scope: new.scope,
                claim: new.claim,
                provenance: new.provenance,
                confidence,
                origin: new.origin,
                author: new.author,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
                expires_at_ms: new.expires_at_ms,
                status: MemoryStatus::Active,
                supersedes: new.supersedes,
                superseded_by: None,
                revocation: None,
            },
        );
        Ok(id)
    }

    /// Withdraw a record. The tombstone is the record itself, kept with its
    /// reason: an audit needs to see what was believed, not a hole where it was.
    pub fn revoke(
        &mut self,
        id: MemoryId,
        by: PolicySource,
        reason: impl Into<String>,
        now_ms: u64,
    ) -> Result<(), MemoryError> {
        let existing = self.records.get(&id).ok_or(MemoryError::Unknown(id))?;
        self.check_authority(existing, by, "cannot revoke")?;
        if existing.status == MemoryStatus::Revoked {
            return Err(MemoryError::NotActive(id));
        }
        let existing = self.records.get_mut(&id).expect("checked above");
        existing.status = MemoryStatus::Revoked;
        existing.revocation = Some(reason.into());
        existing.updated_at_ms = now_ms;
        Ok(())
    }

    /// Live records for a scope, most confident first and most recent within
    /// that.
    ///
    /// Conflicting records are all returned: two live claims in one scope are a
    /// conflict the caller has to see, not something to resolve by picking one.
    pub fn recall(&self, scope: &MemoryScope, now_ms: u64) -> Vec<&MemoryRecord> {
        let mut hits: Vec<&MemoryRecord> = self
            .records
            .values()
            .filter(|record| record.scope == *scope && record.is_live(now_ms))
            .collect();
        hits.sort_by(|left, right| {
            right
                .confidence
                .cmp(&left.confidence)
                .then(right.updated_at_ms.cmp(&left.updated_at_ms))
                .then(left.id.cmp(&right.id))
        });
        hits
    }

    /// Records that have expired but are still marked active, so a caller can
    /// tombstone them on a schedule instead of at read time.
    pub fn expired(&self, now_ms: u64) -> Vec<MemoryId> {
        self.records
            .values()
            .filter(|record| {
                record.status == MemoryStatus::Active
                    && record.expires_at_ms.is_some_and(|expiry| now_ms >= expiry)
            })
            .map(|record| record.id)
            .collect()
    }

    /// An origin may act on a record its own authority covers, and no other.
    fn check_authority(
        &self,
        existing: &MemoryRecord,
        by: PolicySource,
        verb: &str,
    ) -> Result<(), MemoryError> {
        // `PolicySource` is ordered most authoritative first, so "at least as
        // authoritative" is `<=`.
        if by <= existing.origin {
            return Ok(());
        }
        Err(MemoryError::NotPermitted {
            origin: by,
            reason: format!("{verb} a {} record", existing.origin),
        })
    }
}

/// Refuse a claim that carries a credential.
///
/// Two checks, because they catch different things: the redactor knows the
/// credentials this process has resolved, and the shape check catches one it
/// has never seen. Neither is complete on its own, and a memory store is the
/// wrong place to be approximately right about secrets.
fn reject_secret_like(claim: &str, redactor: &Redactor) -> Result<(), MemoryError> {
    match redactor.sanitize(claim) {
        Ok(sanitized) if sanitized != claim => {
            return Err(MemoryError::SecretLike(
                "it contains a stored credential".to_owned(),
            ))
        }
        Ok(_) => {}
        Err(error) => return Err(MemoryError::SecretLike(error.to_string())),
    }
    for token in claim.split(|character: char| {
        !(character.is_alphanumeric() || character == '_' || character == '-')
    }) {
        if let Some(prefix) = KNOWN_KEY_PREFIXES
            .iter()
            .find(|prefix| token.starts_with(**prefix))
        {
            return Err(MemoryError::SecretLike(format!(
                "a token begins with `{prefix}`"
            )));
        }
        if token.len() >= 40 && is_high_entropy(token) {
            return Err(MemoryError::SecretLike(
                "a long mixed-case alphanumeric token".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Prefixes vendors publish precisely so a key can be recognized in text.
const KNOWN_KEY_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "pk_",
    "rk_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "ASIA",
    "AIza",
    "ya29.",
];

/// A long token using upper case, lower case, and digits at once. Prose does
/// not produce those; encoded key material does.
fn is_high_entropy(token: &str) -> bool {
    token.bytes().any(|byte| byte.is_ascii_uppercase())
        && token.bytes().any(|byte| byte.is_ascii_lowercase())
        && token.bytes().any(|byte| byte.is_ascii_digit())
        && token.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// The stream a workspace's memory lives in.
///
/// A fixed id rather than a session's, because memory outlives the session
/// that wrote it: a repository fact learned in one run has to be there for the
/// next. It is the same store, so memory inherits the durability, the export,
/// and the audit trail the sessions already have instead of inventing a second
/// place to keep things.
pub const MEMORY_STREAM: SessionId =
    SessionId::from_uuid(uuid::uuid!("a5c04e11-0000-4000-8000-000000000001"));

/// Every mutation appends the records it touched, whole.
///
/// A record is small and a supersession changes two of them, so writing the
/// records rather than a delta keeps replay to "the last version of each id"
/// — no second implementation of the rules that produced them, and therefore
/// no way for replay to disagree with the write that it is replaying.
const MEMORY_RECORDED: &str = "memory.recorded";

/// The memory of one workspace, kept in its event store.
pub struct MemoryStore {
    store: Arc<dyn EventStore>,
    index: MemoryIndex,
    version: StreamVersion,
}

impl MemoryStore {
    /// Replay the memory stream into an index.
    pub fn open(store: Arc<dyn EventStore>) -> Result<Self, StoreError> {
        let mut records: BTreeMap<MemoryId, MemoryRecord> = BTreeMap::new();
        let mut next = 1;
        loop {
            let page = store.read(MEMORY_STREAM, next, MAX_MEMORY_PAGE)?;
            let Some(last) = page.last() else {
                break;
            };
            next = last.sequence.saturating_add(1);
            for event in &page {
                let EventPayload::Inline { data } = &event.payload else {
                    continue;
                };
                if event.kind != MEMORY_RECORDED {
                    continue;
                }
                if let Ok(record) = serde_json::from_value::<MemoryRecord>(data.clone()) {
                    // Last write wins by sequence, which is how a superseded
                    // record's own update reaches the index.
                    records.insert(record.id, record);
                }
            }
        }
        let version = store.current_version(MEMORY_STREAM)?;
        Ok(Self {
            store,
            index: MemoryIndex::rebuild(records.into_values()),
            version,
        })
    }

    pub const fn index(&self) -> &MemoryIndex {
        &self.index
    }

    /// Record a claim and make it durable.
    pub fn remember(
        &mut self,
        new: NewMemory,
        claim_text: &str,
        redactor: &Redactor,
        now_ms: u64,
    ) -> Result<MemoryId, MemoryError> {
        let superseded = new.supersedes;
        let id = self.index.remember(new, claim_text, redactor, now_ms)?;
        self.append(&[Some(id), superseded])
            .map_err(|error| MemoryError::Storage(error.to_string()))?;
        Ok(id)
    }

    pub fn revoke(
        &mut self,
        id: MemoryId,
        by: PolicySource,
        reason: impl Into<String>,
        now_ms: u64,
    ) -> Result<(), MemoryError> {
        self.index.revoke(id, by, reason, now_ms)?;
        self.append(&[Some(id)])
            .map_err(|error| MemoryError::Storage(error.to_string()))
    }

    /// Tombstone everything whose expiry has passed.
    ///
    /// Expiry is a lifecycle decision, not a read-time filter: a record that
    /// has run out should leave the store's answer *and* say when it did, and
    /// `is_live` alone would silently keep it forever.
    pub fn expire(&mut self, now_ms: u64) -> Result<Vec<MemoryId>, MemoryError> {
        let expired = self.index.expired(now_ms);
        for id in &expired {
            let origin = self
                .index
                .get(*id)
                .map(|record| record.origin)
                .ok_or(MemoryError::Unknown(*id))?;
            self.index.revoke(*id, origin, "expired", now_ms)?;
        }
        self.append(&expired.iter().copied().map(Some).collect::<Vec<_>>())
            .map_err(|error| MemoryError::Storage(error.to_string()))?;
        Ok(expired)
    }

    fn append(&mut self, touched: &[Option<MemoryId>]) -> Result<(), StoreError> {
        let mut events = Vec::new();
        let mut sequence = self.version.0;
        for id in touched.iter().flatten() {
            let Some(record) = self.index.get(*id) else {
                continue;
            };
            sequence = sequence
                .checked_add(1)
                .ok_or(StoreError::SequenceOverflow)?;
            events.push(EventEnvelope::new(
                MEMORY_STREAM,
                sequence,
                record.author.clone(),
                None,
                CorrelationId::new(),
                SchemaVersion(1),
                MEMORY_RECORDED,
                EventPayload::Inline {
                    data: serde_json::to_value(record)
                        .map_err(|error| StoreError::Serialization(error.to_string()))?,
                },
            ));
        }
        if events.is_empty() {
            return Ok(());
        }
        self.version = self.store.append(MEMORY_STREAM, self.version, events)?;
        Ok(())
    }
}

const MAX_MEMORY_PAGE: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::SecretHandle;

    fn new(scope: MemoryScope, origin: PolicySource, confidence: Confidence) -> NewMemory {
        NewMemory {
            scope,
            claim: ArtifactId::new(),
            provenance: vec![ArtifactId::new()],
            confidence,
            origin,
            author: Principal::User("dev".into()),
            expires_at_ms: None,
            supersedes: None,
        }
    }

    #[test]
    fn repository_text_cannot_create_trusted_memory() {
        let mut index = MemoryIndex::new();
        let redactor = Redactor::new();
        let repository = index
            .remember(
                new(
                    MemoryScope::Repository,
                    PolicySource::Workspace,
                    Confidence::Verified,
                ),
                "the build uses cargo",
                &redactor,
                1,
            )
            .unwrap();
        assert_eq!(
            index.get(repository).unwrap().confidence,
            Confidence::Reported,
            "an untrusted origin cannot certify its own claim"
        );

        // A claim resting on nothing is reported at best, whoever wrote it.
        let unsupported = index
            .remember(
                NewMemory {
                    provenance: Vec::new(),
                    ..new(MemoryScope::User, PolicySource::User, Confidence::Verified)
                },
                "the operator prefers tabs",
                &redactor,
                1,
            )
            .unwrap();
        assert_eq!(
            index.get(unsupported).unwrap().confidence,
            Confidence::Reported
        );

        let user = index
            .remember(
                new(MemoryScope::User, PolicySource::User, Confidence::Verified),
                "the operator prefers spaces",
                &redactor,
                1,
            )
            .unwrap();
        assert_eq!(index.get(user).unwrap().confidence, Confidence::Verified);

        // A workspace record cannot revoke or supersede a user one.
        assert_eq!(
            index.revoke(user, PolicySource::Workspace, "no", 2),
            Err(MemoryError::NotPermitted {
                origin: PolicySource::Workspace,
                reason: "cannot revoke a user record".to_owned(),
            })
        );
        assert_eq!(
            index.remember(
                NewMemory {
                    supersedes: Some(user),
                    ..new(
                        MemoryScope::User,
                        PolicySource::Workspace,
                        Confidence::Reported
                    )
                },
                "actually tabs",
                &redactor,
                2,
            ),
            Err(MemoryError::NotPermitted {
                origin: PolicySource::Workspace,
                reason: "cannot supersede a user record".to_owned(),
            })
        );
        assert_eq!(index.get(user).unwrap().status, MemoryStatus::Active);
    }

    #[test]
    fn superseding_leaves_a_chain_and_revoking_leaves_a_tombstone() {
        let mut index = MemoryIndex::new();
        let redactor = Redactor::new();
        let first = index
            .remember(
                new(MemoryScope::User, PolicySource::User, Confidence::Observed),
                "port 8080",
                &redactor,
                1,
            )
            .unwrap();
        let second = index
            .remember(
                NewMemory {
                    supersedes: Some(first),
                    ..new(MemoryScope::User, PolicySource::User, Confidence::Observed)
                },
                "port 9090",
                &redactor,
                2,
            )
            .unwrap();

        assert_eq!(index.get(first).unwrap().status, MemoryStatus::Superseded);
        assert_eq!(index.get(first).unwrap().superseded_by, Some(second));
        assert_eq!(index.get(second).unwrap().supersedes, Some(first));
        assert_eq!(
            index
                .recall(&MemoryScope::User, 3)
                .iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            vec![second],
            "a superseded record leaves retrieval"
        );
        // Superseding it twice is refused: the chain has one head.
        assert_eq!(
            index.remember(
                NewMemory {
                    supersedes: Some(first),
                    ..new(MemoryScope::User, PolicySource::User, Confidence::Observed)
                },
                "port 7070",
                &redactor,
                3,
            ),
            Err(MemoryError::NotActive(first))
        );
        // And it cannot move a fact into another scope on the way.
        assert_eq!(
            index.remember(
                NewMemory {
                    supersedes: Some(second),
                    ..new(
                        MemoryScope::Repository,
                        PolicySource::User,
                        Confidence::Observed
                    )
                },
                "port 7070",
                &redactor,
                3,
            ),
            Err(MemoryError::ScopeMismatch)
        );

        index
            .revoke(second, PolicySource::User, "wrong", 4)
            .unwrap();
        assert!(index.recall(&MemoryScope::User, 5).is_empty());
        let tombstone = index.get(second).unwrap();
        assert_eq!(tombstone.status, MemoryStatus::Revoked);
        assert_eq!(tombstone.revocation.as_deref(), Some("wrong"));
        assert_eq!(index.all().count(), 2, "nothing was deleted");
        assert_eq!(
            index.revoke(second, PolicySource::User, "again", 5),
            Err(MemoryError::NotActive(second))
        );
    }

    #[test]
    fn conflicting_live_records_are_returned_together_and_expiry_is_reported() {
        let mut index = MemoryIndex::new();
        let redactor = Redactor::new();
        let reported = index
            .remember(
                new(
                    MemoryScope::Repository,
                    PolicySource::Workspace,
                    Confidence::Reported,
                ),
                "the tests are slow",
                &redactor,
                1,
            )
            .unwrap();
        let verified = index
            .remember(
                new(
                    MemoryScope::Repository,
                    PolicySource::User,
                    Confidence::Verified,
                ),
                "the tests are fast",
                &redactor,
                2,
            )
            .unwrap();
        let recalled = index.recall(&MemoryScope::Repository, 3);
        assert_eq!(
            recalled.iter().map(|record| record.id).collect::<Vec<_>>(),
            vec![verified, reported],
            "both are returned, most confident first; the conflict is the answer"
        );

        // Another scope sees none of it.
        assert!(index.recall(&MemoryScope::User, 3).is_empty());

        let expiring = index
            .remember(
                NewMemory {
                    expires_at_ms: Some(10),
                    ..new(
                        MemoryScope::Working,
                        PolicySource::User,
                        Confidence::Observed,
                    )
                },
                "the branch is checked out",
                &redactor,
                1,
            )
            .unwrap();
        assert_eq!(index.recall(&MemoryScope::Working, 9).len(), 1);
        assert!(index.recall(&MemoryScope::Working, 10).is_empty());
        assert_eq!(index.expired(10), vec![expiring]);
        assert!(index.expired(9).is_empty());
    }

    #[test]
    fn a_claim_that_carries_a_credential_is_refused() {
        let mut index = MemoryIndex::new();
        let mut redactor = Redactor::new();
        let handle = SecretHandle::new("file", "openai.key").unwrap();
        redactor
            .register(&handle, "a-very-long-stored-credential")
            .unwrap();

        for refused in [
            "the key is a-very-long-stored-credential",
            "use sk-abcdefghijklmnopqrstuvwxyz012345",
            "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
            "aws key AKIAIOSFODNN7EXAMPLE",
            // Never seen before, but shaped like key material.
            "the value is Zm9vYmFyQmF6UXV4MTIzNDU2Nzg5MGFiY2RlZmdo",
        ] {
            assert!(
                matches!(
                    index.remember(
                        new(MemoryScope::User, PolicySource::User, Confidence::Observed),
                        refused,
                        &redactor,
                        1,
                    ),
                    Err(MemoryError::SecretLike(_))
                ),
                "{refused}"
            );
        }
        // Ordinary prose, including long words and hyphenated identifiers, is
        // not mistaken for key material.
        for allowed in [
            "the deployment pipeline runs on the release branch",
            "see crates/arsy-kernel/src/memory.rs for the record type",
            "the incantation is supercalifragilisticexpialidocious, apparently",
        ] {
            assert!(
                index
                    .remember(
                        new(MemoryScope::User, PolicySource::User, Confidence::Observed),
                        allowed,
                        &redactor,
                        1,
                    )
                    .is_ok(),
                "{allowed}"
            );
        }
    }

    #[test]
    fn an_index_rebuilds_to_the_same_state() {
        let mut index = MemoryIndex::new();
        let redactor = Redactor::new();
        for claim in ["one", "two", "three"] {
            index
                .remember(
                    new(
                        MemoryScope::Repository,
                        PolicySource::User,
                        Confidence::Observed,
                    ),
                    claim,
                    &redactor,
                    1,
                )
                .unwrap();
        }
        let records: Vec<MemoryRecord> = index.all().cloned().collect();
        let rebuilt = MemoryIndex::rebuild(records.iter().rev().cloned());
        assert_eq!(
            rebuilt.recall(&MemoryScope::Repository, 2),
            index.recall(&MemoryScope::Repository, 2),
            "retrieval does not depend on the order records were read in"
        );
    }

    #[test]
    fn memory_survives_the_process_that_wrote_it_and_keeps_its_tombstones() {
        use crate::event::MemoryEventStore;

        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let redactor = Redactor::new();
        let mut memory = MemoryStore::open(Arc::clone(&store)).unwrap();

        let first = memory
            .remember(
                new(
                    MemoryScope::Repository,
                    PolicySource::User,
                    Confidence::Observed,
                ),
                "the build uses cargo",
                &redactor,
                1,
            )
            .unwrap();
        let mut replacement = new(
            MemoryScope::Repository,
            PolicySource::User,
            Confidence::Verified,
        );
        replacement.supersedes = Some(first);
        let second = memory
            .remember(replacement, "the build uses cargo and make", &redactor, 2)
            .unwrap();
        let expiring = {
            let mut new = new(
                MemoryScope::Working,
                PolicySource::User,
                Confidence::Reported,
            );
            new.expires_at_ms = Some(5);
            memory
                .remember(new, "the branch is green", &redactor, 3)
                .unwrap()
        };

        // A second process reads the same store and believes the same things.
        let reopened = MemoryStore::open(Arc::clone(&store)).unwrap();
        let live = reopened.index().recall(&MemoryScope::Repository, 4);
        assert_eq!(live.len(), 1, "a superseded record leaves retrieval");
        assert_eq!(live[0].id, second);
        assert_eq!(
            reopened.index().get(first).unwrap().status,
            MemoryStatus::Superseded,
            "the record it replaced is still there, marked"
        );
        assert_eq!(reopened.index().recall(&MemoryScope::Working, 4).len(), 1);

        // Expiry is a write, so it survives too, with its reason.
        let mut memory = MemoryStore::open(Arc::clone(&store)).unwrap();
        assert_eq!(memory.expire(6).unwrap(), vec![expiring]);
        let reopened = MemoryStore::open(store).unwrap();
        assert!(reopened.index().recall(&MemoryScope::Working, 6).is_empty());
        let tombstone = reopened.index().get(expiring).unwrap();
        assert_eq!(tombstone.status, MemoryStatus::Revoked);
        assert_eq!(tombstone.revocation.as_deref(), Some("expired"));
    }
}
