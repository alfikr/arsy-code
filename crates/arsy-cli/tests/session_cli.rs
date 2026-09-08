//! End-to-end checks for `arsy session` against a real recorded store.
//!
//! These drive the built binary rather than the library so the argument
//! parsing, the store path, and the JSON record are all exercised the way an
//! operator or a script reaches them.

use arsy_kernel::{
    domain::{Principal, SessionId},
    event::EventStore,
    protocol::{ClientRequest, Extensions, ProtocolEnvelope, TurnStart},
    service::AgentService,
    sqlite::{Durability, SqliteEventStore},
};
use serde_json::Value;
use std::{path::Path, process::Command, sync::Arc};

/// Record one completed turn and one running turn, and hand back the session.
fn recorded(workspace: &Path) -> (SessionId, Vec<String>) {
    let path = workspace.join(".arsy/sessions.sqlite3");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let store: Arc<dyn EventStore> =
        Arc::new(SqliteEventStore::open(&path, Durability::Normal).unwrap());
    let session = SessionId::new();
    let service = AgentService::attach(Arc::clone(&store), session).unwrap();
    let actor = Principal::User("tester".into());
    let start = |prompt: &str| {
        ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
            session,
            prompt: prompt.to_owned(),
            extensions: Extensions::new(),
        }))
    };
    let first = service.start_turn(actor.clone(), &start("one")).unwrap();
    service
        .complete_turn(actor.clone(), first.turn, &serde_json::json!({"ok": true}))
        .unwrap();
    service.start_turn(actor, &start("two")).unwrap();

    let events = AgentService::history(store.as_ref(), session)
        .unwrap()
        .into_iter()
        .map(|event| event.id.to_string())
        .collect();
    (session, events)
}

fn arsy(workspace: &Path, args: &[&str]) -> (i32, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_arsy"))
        .args(["--workspace", workspace.to_str().unwrap()])
        .args(args)
        .args(["--output", "json"])
        .output()
        .expect("the binary runs");
    let stdout = String::from_utf8(output.stdout).expect("machine output is UTF-8");
    let record: Value = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["type"] == "result")
        .unwrap_or_else(|| panic!("no result record in {stdout}"));
    (
        output.status.code().unwrap_or(-1),
        record["payload"].clone(),
    )
}

#[test]
fn a_recorded_session_lists_shows_and_exports() {
    let workspace = tempfile::tempdir().unwrap();
    let (session, events) = recorded(workspace.path());
    let id = session.to_string();

    let (code, listed) = arsy(workspace.path(), &["session", "list"]);
    assert_eq!(code, 0);
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    let row = &listed["sessions"][0];
    assert_eq!(row["session"], id);
    assert_eq!(row["events"], 3);
    assert_eq!(row["turns"], 2);
    // One turn is still running, so the session is running however many
    // turns finished before it.
    assert_eq!(row["status"], "running");

    let (code, shown) = arsy(
        workspace.path(),
        &["session", "show", &id, "--turns", "--evidence"],
    );
    assert_eq!(code, 0);
    assert_eq!(shown["turn_detail"].as_array().unwrap().len(), 2);
    assert_eq!(shown["timeline"].as_array().unwrap().len(), 3);
    assert_eq!(shown["timeline"][0]["kind"], "turn.started");
    assert_eq!(shown["branched_from"], Value::Null);

    let out = workspace.path().join("audit.jsonl");
    let (code, exported) = arsy(
        workspace.path(),
        &["session", "export", &id, "--out", out.to_str().unwrap()],
    );
    assert_eq!(code, 0);
    assert_eq!(exported["events"], 3);
    let lines: Vec<Value> = std::fs::read_to_string(&out)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["id"], events[0]);
    assert_eq!(lines[2]["sequence"], 3);

    // An ID this workspace never recorded is refused rather than reported as
    // an empty session.
    let (code, _) = arsy(
        workspace.path(),
        &["session", "show", &SessionId::new().to_string()],
    );
    assert_eq!(code, 2);
}

#[test]
fn rewinding_and_forking_branch_without_touching_the_parent() {
    let workspace = tempfile::tempdir().unwrap();
    let (session, events) = recorded(workspace.path());
    let id = session.to_string();

    let (code, rewound) = arsy(
        workspace.path(),
        &["session", "rewind", &id, "--to", &events[0]],
    );
    assert_eq!(code, 0);
    assert_eq!(rewound["parent"], id);
    assert_eq!(rewound["mode"], "rewind");
    assert_eq!(rewound["at_sequence"], 1);
    assert_eq!(rewound["parent_version"], 3);
    assert_eq!(rewound["inherits_prefix"], true);

    let (code, forked) = arsy(workspace.path(), &["session", "fork", &id]);
    assert_eq!(code, 0);
    assert_eq!(
        forked["at_sequence"], 3,
        "a fork defaults to the parent head"
    );
    assert_eq!(forked["inherits_prefix"], false);

    // The parent is untouched, and both branches are now recorded beside it.
    let (_, shown) = arsy(workspace.path(), &["session", "show", &id]);
    assert_eq!(shown["events"], 3);
    let (_, listed) = arsy(workspace.path(), &["session", "list"]);
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 3);

    let branch = rewound["session"].as_str().unwrap();
    let (_, shown) = arsy(workspace.path(), &["session", "show", branch]);
    assert_eq!(shown["branched_from"]["session"], id);
    assert_eq!(shown["branched_from"]["at_event"], events[0]);

    // A branch point from another session is not a branch point here.
    let (code, _) = arsy(
        workspace.path(),
        &[
            "session",
            "rewind",
            &id,
            "--to",
            "00000000-0000-4000-8000-000000000000",
        ],
    );
    assert_eq!(code, 2);
}

#[test]
fn gc_reports_before_it_removes() {
    let workspace = tempfile::tempdir().unwrap();
    recorded(workspace.path());

    let (code, dry) = arsy(workspace.path(), &["gc"]);
    assert_eq!(code, 0);
    assert_eq!(dry["applied"], false);
    assert_eq!(dry["unreachable_references"], 0);
    assert_eq!(dry["orphan_objects"], 0);
    assert!(dry.get("removed_references").is_none());

    let (code, applied) = arsy(workspace.path(), &["gc", "--apply", "--retention", "1h"]);
    assert_eq!(code, 0);
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["retention_ms"], 3_600_000);
    assert_eq!(applied["removed_references"], 0);
}

#[test]
fn an_artifact_shows_bounded_exports_whole_and_is_collected_when_unreachable() {
    use arsy_kernel::artifact::{ArtifactStore, FileArtifactStore, NewArtifact, Sensitivity};

    let workspace = tempfile::tempdir().unwrap();
    recorded(workspace.path());
    let store = FileArtifactStore::open(workspace.path().join(".arsy/artifacts"), 0).unwrap();
    let body = "line one\nline two\n".repeat(64);
    let stored = store
        .put(
            body.as_bytes(),
            NewArtifact {
                media_type: "text/plain".into(),
                creator: Principal::User("tester".into()),
                source_revision: None,
                sensitivity: Sensitivity::Internal,
                // Already past retention, so `gc` can consider it immediately.
                retain_until_ms: 0,
            },
        )
        .unwrap();
    let reference = format!("artifact://{}", stored.id);

    let (code, bounded) = arsy(
        workspace.path(),
        &["artifact", "show", &reference, "--max-bytes", "4096"],
    );
    assert_eq!(code, 0);
    assert_eq!(bounded["metadata"]["media_type"], "text/plain");
    assert_eq!(bounded["rendered"]["encoding"], "utf-8");
    assert_eq!(bounded["truncated"], false);
    assert!(bounded["rendered"]["content"]
        .as_str()
        .unwrap()
        .starts_with("line one"));

    // A bound below the stored size refuses rather than printing a prefix that
    // would read as the whole artifact.
    let (code, _) = arsy(
        workspace.path(),
        &["artifact", "show", &reference, "--max-bytes", "8"],
    );
    assert_eq!(code, 6);

    let out = workspace.path().join("evidence.txt");
    let (code, exported) = arsy(
        workspace.path(),
        &[
            "artifact",
            "export",
            &reference,
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(exported["bytes"], body.len());
    assert_eq!(exported["redaction"]["scanned"], true);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), body);

    // No event references it, so it is collectable — but only with --apply.
    let (_, dry) = arsy(workspace.path(), &["gc"]);
    assert_eq!(dry["unreachable_references"], 1);
    assert_eq!(dry["references"][0]["artifact"], stored.id.to_string());
    assert!(
        store.metadata(stored.id).is_ok(),
        "a dry run removes nothing"
    );

    let (_, applied) = arsy(workspace.path(), &["gc", "--apply", "--retention", "0s"]);
    assert_eq!(applied["removed_references"], 1);
    assert_eq!(applied["removed_objects"], 1);
    assert!(store.metadata(stored.id).is_err());

    let (code, _) = arsy(workspace.path(), &["artifact", "show", &reference]);
    assert_eq!(code, 6);
}
