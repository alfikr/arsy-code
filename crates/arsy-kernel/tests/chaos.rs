//! Crash-path acceptance checks for the durable event store and agent service.

use arsy_kernel::{
    domain::{CorrelationId, Principal, SessionId},
    event::{EventEnvelope, EventPayload, EventStore, SchemaVersion, StreamVersion},
    protocol::{ClientRequest, IdempotencyKey, ProtocolEnvelope, TurnStart},
    service::AgentService,
    sqlite::{Durability, SqliteEventStore},
};
use rusqlite::Connection;
use serde_json::Value;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const CHILD_DB: &str = "ARSY_CHAOS_DB";
const CHILD_READY: &str = "ARSY_CHAOS_READY";
const CHILD_SESSION: &str = "ARSY_CHAOS_SESSION";

fn event(session: SessionId, sequence: u64) -> EventEnvelope {
    EventEnvelope::new(
        session,
        sequence,
        Principal::System,
        None,
        CorrelationId::new(),
        SchemaVersion(1),
        "chaos.event",
        EventPayload::Inline { data: Value::Null },
    )
}

#[test]
#[ignore = "subprocess helper invoked by the crash test"]
fn crash_writer() {
    let path = PathBuf::from(env::var_os(CHILD_DB).expect("database path"));
    let ready = PathBuf::from(env::var_os(CHILD_READY).expect("ready path"));
    let session = env::var(CHILD_SESSION)
        .expect("session ID")
        .parse::<SessionId>()
        .expect("valid session ID");
    let store = SqliteEventStore::open(path, Durability::Strict).unwrap();

    for _ in 0..256 {
        fs::write(&ready, b"ready").unwrap();
        thread::sleep(Duration::from_millis(20));
        let version = store.current_version(session).unwrap();
        let batch = (1..=32)
            .map(|offset| event(session, version.0 + offset))
            .collect();
        store.append(session, version, batch).unwrap();
    }
}

fn wait_until_exists(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "child did not reach append point"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn crashes_truncation_and_duplicates_preserve_committed_history() {
    for delay_ms in [0, 10, 25] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.sqlite3");
        let ready = directory.path().join("ready");
        let session = SessionId::new();
        let store = SqliteEventStore::open(&path, Durability::Strict).unwrap();
        store
            .append(session, StreamVersion(0), vec![event(session, 1)])
            .unwrap();
        drop(store);

        let mut child = Command::new(env::current_exe().unwrap())
            .args(["--ignored", "--exact", "crash_writer"])
            .env(CHILD_DB, &path)
            .env(CHILD_READY, &ready)
            .env(CHILD_SESSION, session.to_string())
            .spawn()
            .unwrap();
        wait_until_exists(&ready);
        thread::sleep(Duration::from_millis(delay_ms));
        child.kill().unwrap();
        child.wait().unwrap();

        let reopened = SqliteEventStore::open(&path, Durability::Strict).unwrap();
        let committed = reopened.current_version(session).unwrap();
        let replay = reopened.read(session, 1, 10_000).unwrap();
        assert_eq!(replay.len() as u64, committed.0);
        assert_eq!(replay.first().unwrap().sequence, 1);
        assert!(replay
            .iter()
            .enumerate()
            .all(|(index, event)| event.sequence == index as u64 + 1));

        reopened
            .append(session, committed, vec![event(session, committed.0 + 1)])
            .unwrap();
        assert_eq!(
            reopened.current_version(session).unwrap().0,
            committed.0 + 1
        );
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("truncated.sqlite3");
    let session = SessionId::new();
    let store = SqliteEventStore::open(&path, Durability::Normal).unwrap();
    store
        .append(
            session,
            StreamVersion(0),
            (1..=3).map(|sequence| event(session, sequence)).collect(),
        )
        .unwrap();
    drop(store);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE events SET envelope_json = substr(envelope_json, 1, 8) \
             WHERE stream_id = ?1 AND sequence = 3",
            [session.to_string()],
        )
        .unwrap();
    drop(connection);
    let reopened = SqliteEventStore::open(&path, Durability::Normal).unwrap();
    assert_eq!(reopened.read(session, 1, 2).unwrap().len(), 2);
    assert!(reopened.read(session, 1, 3).is_err());

    let session = SessionId::new();
    let store = Arc::new(
        SqliteEventStore::open(
            directory.path().join("duplicate.sqlite3"),
            Durability::Strict,
        )
        .unwrap(),
    );
    let service = AgentService::attach(store.clone(), session).unwrap();
    let request = ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
        session,
        prompt: "run once".into(),
        extensions: Default::default(),
    }))
    .with_idempotency_key(IdempotencyKey::new("chaos-retry").unwrap());
    let first = service.start_turn(Principal::System, &request).unwrap();
    let duplicate = service.start_turn(Principal::System, &request).unwrap();
    assert_eq!(duplicate.turn, first.turn);
    assert!(duplicate.replay);
    assert_eq!(store.current_version(session).unwrap(), StreamVersion(1));
}
