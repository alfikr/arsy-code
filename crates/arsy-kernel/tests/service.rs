//! Acceptance checks for the embedded agent service (docs/04-system-architecture.md).

use arsy_kernel::{
    domain::{Principal, SessionId, SubscriptionId, TurnId},
    event::{EventStore, MemoryEventStore},
    protocol::{
        ClientRequest, IdempotencyKey, ProtocolEnvelope, ServerEvent, TurnStart,
        MAX_SUBSCRIPTION_BATCH,
    },
    service::{AgentService, ServiceError, TurnFinishedEvidence, TurnStartedEvidence},
    sqlite::{Durability, SqliteEventStore},
};
use serde_json::json;
use std::sync::Arc;

fn turn_start(session: SessionId, prompt: &str) -> ProtocolEnvelope<ClientRequest> {
    ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
        session,
        prompt: prompt.to_owned(),
        extensions: Default::default(),
    }))
}

fn subscription_of(events: &[ServerEvent]) -> SubscriptionId {
    match events.first().expect("subscribed event") {
        ServerEvent::Subscribed { subscription, .. } => *subscription,
        other => panic!("expected subscribed, got {other:?}"),
    }
}

#[test]
fn turn_lifecycle_appends_started_and_completed_with_evidence() {
    let store = Arc::new(MemoryEventStore::default());
    let session = SessionId::new();
    let service = AgentService::attach(store.clone(), session).unwrap();

    let request = turn_start(session, "explain the edit engine");
    let admitted = service
        .start_turn(Principal::User("dev".into()), &request)
        .unwrap();
    assert!(!admitted.replay);

    let outcome = json!({ "status": "ok" });
    service
        .complete_turn(Principal::System, admitted.turn, &outcome)
        .unwrap();

    let events = store.read(session, 1, 16).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, "turn.started");
    assert_eq!(events[1].kind, "turn.completed");
    assert_eq!(events[1].causation, Some(events[0].id));

    let started: TurnStartedEvidence = payload(&events[0]);
    assert_eq!(started.turn_id, admitted.turn);
    assert_eq!(started.prompt, "explain the edit engine");
    assert_eq!(started.request_digest, request.request_digest().unwrap());

    let finished: TurnFinishedEvidence = payload(&events[1]);
    assert_eq!(finished.turn_id, admitted.turn);
    assert_eq!(finished.started_sequence, events[0].sequence);
    assert!(finished.failure.is_none());
    assert!(service.unfinished_turns().unwrap().is_empty());

    // A finished turn cannot be finished twice.
    assert_eq!(
        service.complete_turn(Principal::System, admitted.turn, &outcome),
        Err(ServiceError::TurnNotRunning(admitted.turn))
    );
    let unknown = TurnId::new();
    assert_eq!(
        service.complete_turn(Principal::System, unknown, &outcome),
        Err(ServiceError::UnknownTurn(unknown))
    );
}

#[test]
fn duplicate_idempotency_key_executes_once_and_conflicts_on_a_different_body() {
    let store = Arc::new(MemoryEventStore::default());
    let session = SessionId::new();
    let service = AgentService::attach(store.clone(), session).unwrap();
    let key = IdempotencyKey::new("turn-1").unwrap();

    let request = turn_start(session, "run the tests").with_idempotency_key(key.clone());
    let first = service
        .start_turn(Principal::User("dev".into()), &request)
        .unwrap();
    assert!(!first.replay);

    // Same key, same body, fresh request id: a retry, not a second turn.
    let retry = turn_start(session, "run the tests").with_idempotency_key(key.clone());
    let second = service
        .start_turn(Principal::User("dev".into()), &retry)
        .unwrap();
    assert_eq!(second.turn, first.turn);
    assert!(second.replay);
    assert_eq!(store.read(session, 1, 16).unwrap().len(), 1);

    // Same key, different body: refused rather than silently replayed.
    let forged = turn_start(session, "rm -rf /").with_idempotency_key(key);
    assert!(matches!(
        service.start_turn(Principal::User("dev".into()), &forged),
        Err(ServiceError::Protocol(_))
    ));
    assert_eq!(store.read(session, 1, 16).unwrap().len(), 1);

    // A request for another session is refused before anything is appended.
    let other = turn_start(SessionId::new(), "elsewhere");
    assert_eq!(
        service.start_turn(Principal::System, &other),
        Err(ServiceError::WrongSession)
    );
    assert_eq!(store.read(session, 1, 16).unwrap().len(), 1);
}

#[test]
fn slow_subscriber_gets_a_bounded_gap_and_never_blocks_commits() {
    let store = Arc::new(MemoryEventStore::default());
    let session = SessionId::new();
    let service = AgentService::attach(store.clone(), session).unwrap();

    let subscription = subscription_of(&service.subscribe(1).unwrap());

    // Commit more turns than one subscriber may buffer, without ever polling.
    let commits = MAX_SUBSCRIPTION_BATCH + 8;
    for index in 0..commits {
        let request = turn_start(session, &format!("turn {index}"));
        let turn = service
            .start_turn(Principal::System, &request)
            .unwrap()
            .turn;
        service
            .complete_turn(Principal::System, turn, &json!({ "index": index }))
            .unwrap();
    }
    // Commits succeeded despite the idle subscriber.
    assert_eq!(service.committed_version().unwrap().0, (commits as u64) * 2);

    let delivered = service.poll(subscription).unwrap();
    assert!(delivered.len() <= MAX_SUBSCRIPTION_BATCH + 1);
    let ServerEvent::Gap {
        dropped_from,
        next_sequence,
        ..
    } = delivered[0]
    else {
        panic!("expected a gap first, got {:?}", delivered[0]);
    };
    assert_eq!(dropped_from, 1);
    assert!(next_sequence > 1);

    // After the gap the subscriber is contiguous again from next_sequence.
    let request = turn_start(session, "after the gap");
    service.start_turn(Principal::System, &request).unwrap();
    let resumed = service.poll(subscription).unwrap();
    assert_eq!(resumed.len(), 1);
    let ServerEvent::Stream { envelope, .. } = &resumed[0] else {
        panic!("expected a stream event, got {:?}", resumed[0]);
    };
    assert_eq!(envelope.sequence, service.committed_version().unwrap().0);

    assert!(matches!(
        service.poll(SubscriptionId::new()),
        Err(ServiceError::UnknownSubscription(_))
    ));
}

#[test]
fn a_crash_mid_turn_resumes_from_the_last_committed_event() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.sqlite3");
    let session = SessionId::new();
    let key = IdempotencyKey::new("resume-me").unwrap();
    let request = turn_start(session, "long running work").with_idempotency_key(key);

    let turn = {
        let store = Arc::new(SqliteEventStore::open(&path, Durability::Strict).unwrap());
        let service = AgentService::attach(store, session).unwrap();
        service
            .start_turn(Principal::User("dev".into()), &request)
            .unwrap()
            .turn
        // Process dies here: turn.started is committed, turn.completed is not.
    };

    let store = Arc::new(SqliteEventStore::open(&path, Durability::Strict).unwrap());
    let service = AgentService::attach(store.clone(), session).unwrap();
    assert_eq!(service.committed_version().unwrap().0, 1);
    assert_eq!(service.unfinished_turns().unwrap(), vec![turn]);

    // The idempotency ledger survived the restart: the retry does not re-run.
    let resumed = service
        .start_turn(Principal::User("dev".into()), &request)
        .unwrap();
    assert_eq!(resumed.turn, turn);
    assert!(resumed.replay);
    assert_eq!(store.read(session, 1, 16).unwrap().len(), 1);

    // The recovered turn can be closed, and closing it is durable.
    service
        .fail_turn(Principal::System, turn, "interrupted", "process exited")
        .unwrap();
    assert!(service.unfinished_turns().unwrap().is_empty());

    let reopened = AgentService::attach(
        Arc::new(SqliteEventStore::open(&path, Durability::Strict).unwrap()),
        session,
    )
    .unwrap();
    assert!(reopened.unfinished_turns().unwrap().is_empty());
    assert_eq!(reopened.committed_version().unwrap().0, 2);
}

fn payload<T: serde::de::DeserializeOwned>(event: &arsy_kernel::event::EventEnvelope) -> T {
    let arsy_kernel::event::EventPayload::Inline { data } = &event.payload else {
        panic!("expected inline evidence");
    };
    serde_json::from_value(data.clone()).expect("evidence decodes")
}
