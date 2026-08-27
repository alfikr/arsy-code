//! Property tests for the Phase 1a persistence slice.
//!
//! The exit gate for Phase 1 asks for replay determinism and projection rebuild
//! equivalence, so these properties compare independent paths to the same state
//! rather than asserting hand-written expectations.

use arsy_kernel::{
    artifact::{ArtifactReadLimits, ArtifactStore, FileArtifactStore, NewArtifact, Sensitivity},
    domain::{CorrelationId, EventId, Principal, SessionId, TurnId},
    event::{
        EventEnvelope, EventPayload, EventStore, MemoryEventStore, SchemaVersion, StoreError,
        StreamVersion,
    },
    projection::ProjectionSet,
    sqlite::{Durability, SqliteEventStore},
};
use proptest::prelude::*;
use serde_json::json;
use std::collections::HashSet;
use tempfile::TempDir;
use uuid::Uuid;

/// One generated step of a session, before sequences and identities are assigned.
#[derive(Clone, Debug)]
enum Step {
    StartTurn,
    FinishTurn { pick: usize, completed: bool },
    Usage { input: u64, output: u64, cost: u64 },
    Other(String),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => Just(Step::StartTurn),
        3 => (any::<usize>(), any::<bool>())
            .prop_map(|(pick, completed)| Step::FinishTurn { pick, completed }),
        2 => (0..1_000_000u64, 0..1_000_000u64, 0..1_000_000u64)
            .prop_map(|(input, output, cost)| Step::Usage { input, output, cost }),
        1 => "other\\.[a-z]{1,8}".prop_map(Step::Other),
    ]
}

fn event_stream() -> impl Strategy<Value = (SessionId, Vec<EventEnvelope>)> {
    prop::collection::vec(step(), 0..24).prop_map(build)
}

/// Turn every step into an envelope, keeping the invariants the projection
/// enforces: a turn finishes only once, and only after it started.
///
/// Envelopes are built by struct literal rather than `EventEnvelope::new`
/// because `new` reads the wall clock, which no replay can reproduce.
fn build(steps: Vec<Step>) -> (SessionId, Vec<EventEnvelope>) {
    let session = SessionId::from_uuid(Uuid::from_u128(1));
    let correlation = CorrelationId::from_uuid(Uuid::from_u128(2));
    let mut events: Vec<EventEnvelope> = Vec::new();
    let mut open: Vec<TurnId> = Vec::new();
    let mut started = 0u128;

    for step in steps {
        let (kind, data) = match step {
            Step::StartTurn => {
                started += 1;
                let turn = TurnId::from_uuid(Uuid::from_u128(0x7000_0000 + started));
                open.push(turn);
                ("turn.started".to_owned(), json!({ "turn_id": turn }))
            }
            Step::FinishTurn { pick, completed } => {
                if open.is_empty() {
                    continue;
                }
                let turn = open.remove(pick % open.len());
                let kind = if completed {
                    "turn.completed"
                } else {
                    "turn.failed"
                };
                (kind.to_owned(), json!({ "turn_id": turn }))
            }
            Step::Usage {
                input,
                output,
                cost,
            } => (
                "usage.recorded".to_owned(),
                json!({
                    "input_tokens": input,
                    "output_tokens": output,
                    "cost_micros": cost,
                }),
            ),
            Step::Other(kind) => (kind, json!({})),
        };

        let sequence = events.len() as u64 + 1;
        events.push(EventEnvelope {
            id: EventId::from_uuid(Uuid::from_u128(0x1000_0000 + u128::from(sequence))),
            session,
            sequence,
            occurred_at_ms: 1_700_000_000_000 + sequence,
            actor: Principal::System,
            causation: None,
            correlation,
            schema: SchemaVersion(1),
            kind,
            payload: EventPayload::Inline { data },
        });
    }

    (session, events)
}

fn batch_sizes() -> impl Strategy<Value = Vec<usize>> {
    prop::collection::vec(1usize..5, 1..8)
}

fn append_in_batches(
    store: &dyn EventStore,
    session: SessionId,
    events: &[EventEnvelope],
    sizes: &[usize],
) -> Result<StreamVersion, StoreError> {
    let mut version = StreamVersion(0);
    let mut offset = 0;
    let mut sizes = sizes.iter().copied().cycle();
    while offset < events.len() {
        let take = sizes
            .next()
            .expect("batch sizes are never empty")
            .min(events.len() - offset);
        version = store.append(session, version, events[offset..offset + take].to_vec())?;
        offset += take;
    }
    Ok(version)
}

fn sqlite_store(dir: &TempDir) -> SqliteEventStore {
    SqliteEventStore::open(dir.path().join("events.db"), Durability::Normal)
        .expect("a fresh database always opens")
}

proptest! {
    // A SQLite file per case is not free; 64 cases still explores the shapes
    // that matter without slowing CI down.
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// However the appends are batched, the stream reads back identically.
    #[test]
    fn sqlite_replay_is_independent_of_batching(
        (session, events) in event_stream(),
        sizes in batch_sizes(),
    ) {
        let dir = TempDir::new().unwrap();
        let store = sqlite_store(&dir);
        append_in_batches(&store, session, &events, &sizes).unwrap();

        prop_assert_eq!(store.read(session, 1, usize::MAX).unwrap(), events);
    }

    /// The SQLite store and the in-memory store are the same state machine.
    #[test]
    fn stores_agree_on_version_and_order(
        (session, events) in event_stream(),
        sizes in batch_sizes(),
    ) {
        let dir = TempDir::new().unwrap();
        let sqlite = sqlite_store(&dir);
        let memory = MemoryEventStore::default();
        let sqlite_version = append_in_batches(&sqlite, session, &events, &sizes).unwrap();
        let memory_version = append_in_batches(&memory, session, &events, &sizes).unwrap();

        prop_assert_eq!(sqlite_version, memory_version);
        prop_assert_eq!(
            sqlite.current_version(session).unwrap(),
            memory.current_version(session).unwrap()
        );
        prop_assert_eq!(
            sqlite.read(session, 1, usize::MAX).unwrap(),
            memory.read(session, 1, usize::MAX).unwrap()
        );
    }

    /// Applying incrementally, rebuilding in memory, and rebuilding from the
    /// stored stream all reach the same projection.
    #[test]
    fn projection_rebuild_is_equivalent(
        (session, events) in event_stream(),
        sizes in batch_sizes(),
    ) {
        let dir = TempDir::new().unwrap();
        let store = sqlite_store(&dir);
        append_in_batches(&store, session, &events, &sizes).unwrap();

        let mut incremental = ProjectionSet::new(session);
        for event in &events {
            incremental.apply(event).unwrap();
        }
        let in_memory = ProjectionSet::rebuild(session, &events).unwrap();
        let from_store =
            ProjectionSet::rebuild(session, &store.read(session, 1, usize::MAX).unwrap()).unwrap();

        prop_assert_eq!(&incremental, &in_memory);
        prop_assert_eq!(&incremental, &from_store);
        prop_assert_eq!(incremental.applied_version(), StreamVersion(events.len() as u64));
    }

    /// Rebuilding a prefix and applying the rest matches a full rebuild, which
    /// is what makes checkpointing sound.
    #[test]
    fn partial_rebuild_matches_full_rebuild(
        (session, events) in event_stream(),
        split in any::<prop::sample::Index>(),
    ) {
        let cut = if events.is_empty() { 0 } else { split.index(events.len() + 1) };
        let mut partial = ProjectionSet::rebuild(session, &events[..cut]).unwrap();
        for event in &events[cut..] {
            partial.apply(event).unwrap();
        }

        prop_assert_eq!(partial, ProjectionSet::rebuild(session, &events).unwrap());
    }

    /// A rejected append leaves the stream byte-for-byte as it was.
    #[test]
    fn rejected_append_does_not_mutate_the_stream(
        (session, events) in event_stream(),
        skew in 1u64..8,
    ) {
        let dir = TempDir::new().unwrap();
        let store = sqlite_store(&dir);
        append_in_batches(&store, session, &events, &[3]).unwrap();
        let before = store.read(session, 1, usize::MAX).unwrap();
        let version = store.current_version(session).unwrap();

        let stale = StreamVersion(version.0 + skew);
        let mut extra = build(vec![Step::Other("other.extra".to_owned())]).1;
        extra[0].sequence = stale.0 + 1;
        extra[0].session = session;
        let result = store.append(session, stale, extra);

        let conflicted = matches!(result, Err(StoreError::Conflict { .. }));
        prop_assert!(conflicted, "stale append must be rejected");
        prop_assert_eq!(store.current_version(session).unwrap(), version);
        prop_assert_eq!(store.read(session, 1, usize::MAX).unwrap(), before);
    }

    /// Identical bytes address the same object, and collection never removes an
    /// artifact something still references.
    #[test]
    fn artifact_store_is_content_addressed_and_gc_respects_references(
        bytes in prop::collection::vec(any::<u8>(), 0..4096),
    ) {
        let dir = TempDir::new().unwrap();
        let store = FileArtifactStore::open(dir.path(), 0).unwrap();
        let limits = ArtifactReadLimits { max_bytes: 1 << 20, max_expansion_ratio: 64 };

        let kept = store.put(&bytes, new_artifact()).unwrap();
        let dropped = store.put(&bytes, new_artifact()).unwrap();
        prop_assert_eq!(kept.digest, dropped.digest);
        prop_assert_ne!(kept.id, dropped.id);

        let report = store.gc(&HashSet::from([kept.id, dropped.id]), u64::MAX).unwrap();
        prop_assert_eq!(report.references_removed, 0);
        prop_assert_eq!(report.objects_removed, 0);

        let report = store.gc(&HashSet::from([kept.id]), u64::MAX).unwrap();
        prop_assert_eq!(report.references_removed, 1);
        prop_assert_eq!(report.objects_removed, 0);
        prop_assert_eq!(store.read(kept.id, limits).unwrap(), bytes);
    }
}

fn new_artifact() -> NewArtifact {
    NewArtifact {
        media_type: "application/octet-stream".to_owned(),
        creator: Principal::System,
        source_revision: None,
        sensitivity: Sensitivity::Internal,
        retain_until_ms: 0,
    }
}
