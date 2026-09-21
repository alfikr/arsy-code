//! `arsy verify`: rebuild a session's completion proof and say whether it
//! holds.
//!
//! Read-only, and deliberately separate from the run that produced the work.
//! A harness that decides its own output is verified at the moment it finishes
//! is asserting, not proving. This command reads the same event stream from
//! outside, rebuilds every criterion's verdict from recorded operations, and
//! exits nonzero unless each required one is met by valid, fresh, attributable
//! evidence.
//!
//! The same code runs in CI and on a terminal, and reaches the same verdict
//! after a restart, because nothing it reads is in memory.

use crate::{storage_failed, usage, Command, Diagnostic, Emitter, Invocation, Output};
use arsy_kernel::{
    artifact::FileArtifactStore,
    domain::{Principal, SessionId, TaskId},
    orchestration::TaskGraph,
    proof::{CompletionProof, ProofState, Prover},
    validation::ValidationLog,
};
use serde_json::{json, Value};

/// Artifacts live beside the session store they are evidence for, as
/// `arsy artifact` also assumes.
const ARTIFACT_PATH: &str = ".arsy/artifacts";

pub fn parse(arguments: &crate::ParsedArguments) -> Result<Command, Diagnostic> {
    let session = crate::only_argument(arguments.positional.clone(), "verify", "<SESSION>")?
        .parse::<SessionId>()
        .map_err(|_| usage(VERIFY_HELP))?;
    Ok(Command::Verify { session })
}

const VERIFY_HELP: &str = "usage: arsy verify <SESSION>\n\n\
     Rebuilds the completion proof for every task in a session and reports \
     whether each required acceptance criterion is met by valid, fresh \
     evidence. Exit codes: 0 verified, 1 a criterion failed, 2 evidence is \
     stale, 3 nothing can decide it, 4 unverified or only partly verified.";

pub fn run(
    invocation: &Invocation,
    session: SessionId,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    // Nothing here creates anything. Opening either store would make the
    // directories it expects, and a command whose job is to check a claim
    // must not leave a workspace different from how it found it — least of
    // all one it was pointed at by mistake.
    if !root.join(crate::STORE_PATH).is_file() {
        return Err(no_such_session(
            session,
            "this workspace has no session store",
        ));
    }
    let store = crate::open_store(&root)?;
    let graph =
        TaskGraph::new(store.clone(), session, Principal::System).map_err(storage_failed)?;
    if graph
        .store()
        .current_version(session)
        .map_err(storage_failed)?
        .0
        == 0
    {
        // Reporting `unverified` for a session that is not here would answer
        // a typo with a verdict about the work.
        return Err(no_such_session(
            session,
            "no session with that id is recorded in this workspace",
        ));
    }
    let validations =
        ValidationLog::open(store, session, Principal::System).map_err(storage_failed)?;
    // A missing store is reported rather than assumed empty, because "no
    // artifacts here" and "cannot check artifacts" are different answers —
    // and the second one must not become the first by creating it.
    let artifact_root = root.join(ARTIFACT_PATH);
    let artifacts = artifact_root
        .is_dir()
        .then(|| FileArtifactStore::open(&artifact_root, 0).ok())
        .flatten();

    // The revision the proof answers for is the one the workspace is on now.
    // Evidence recorded against anything else is stale by definition, which
    // is what makes an edit made after a passing check visible here.
    let revision = arsy_code::git::revision(&root);

    let prover = Prover::new(
        &graph,
        validations.records(),
        artifacts
            .as_ref()
            .map(|store| store as &dyn arsy_kernel::artifact::ArtifactStore),
    );
    let now = arsy_kernel::artifact::unix_time_ms();
    let proofs: Vec<CompletionProof> = graph
        .tasks()
        .filter(|task| !graph.criteria_of(task.id).is_empty())
        .map(|task| prover.prove(task.id, revision, now))
        .collect();

    let worst = proofs
        .iter()
        .map(|proof| proof.state)
        .min()
        .unwrap_or(ProofState::Unverified);
    let report = json!({
        "session": session.to_string(),
        "revision": revision.map(|revision| revision.0.to_string()),
        "state": worst,
        "tasks": proofs
            .iter()
            .map(|proof| task_report(&graph, proof))
            .collect::<Vec<_>>(),
        "artifacts_checked": artifacts.is_some(),
    });
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        human(&report)
    });
    // A session with nothing to prove is not proved. Reporting zero here is
    // exactly the "it finished, so it works" claim this command exists to
    // refuse.
    Ok(proofs
        .iter()
        .map(CompletionProof::exit_code)
        .max()
        .unwrap_or_else(|| {
            CompletionProof {
                schema: arsy_kernel::proof::PROOF_SCHEMA_VERSION,
                session,
                task: TaskId::new(),
                revision,
                state: ProofState::Unverified,
                criteria: Vec::new(),
                built_at_ms: now,
            }
            .exit_code()
        }))
}

fn no_such_session(session: SessionId, why: &str) -> Diagnostic {
    Diagnostic::error(
        "ARSY-SCH-1004",
        format!("cannot verify {session}: {why}"),
        "list what is recorded with `arsy session list`, and check --workspace",
    )
}

fn task_report(graph: &TaskGraph, proof: &CompletionProof) -> Value {
    json!({
        "task": proof.task.to_string(),
        "goal": graph.node(proof.task).map(|node| node.goal.clone()),
        "state": proof.state,
        "criteria": proof.criteria.iter().map(|criterion| json!({
            "criterion": criterion.criterion.to_string(),
            "statement": criterion.statement,
            "required": criterion.required,
            "state": criterion.state,
            "why": criterion.why,
            // Said out loud rather than folded into the state: a reader has
            // to be able to tell a check that ran from a person who looked.
            "human_judgment": criterion.human_judgment,
            "evidence": criterion.evidence,
        })).collect::<Vec<_>>(),
    })
}

fn human(report: &Value) -> Value {
    let mut text = format!(
        "session {}\n  verification: {}\n  revision: {}\n",
        report["session"].as_str().unwrap_or("?"),
        report["state"].as_str().unwrap_or("?"),
        report["revision"].as_str().unwrap_or("unknown"),
    );
    if report["artifacts_checked"] != json!(true) {
        text.push_str("  note: no artifact store here, so cited evidence could not be checked\n");
    }
    for task in report["tasks"].as_array().unwrap_or(&Vec::new()) {
        text.push_str(&format!(
            "\n  task {} — {}\n    {}\n{}",
            task["task"].as_str().unwrap_or("?"),
            task["state"].as_str().unwrap_or("?"),
            task["goal"].as_str().unwrap_or(""),
            task["criteria"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .map(criterion_line)
                .collect::<String>(),
        ));
    }
    json!({"text": text})
}

/// One criterion as a person reads it.
fn criterion_line(criterion: &Value) -> String {
    // What decided it, said out loud: a reader has to be able to tell a
    // check that ran from a person who looked, and an optional criterion
    // from one that blocks.
    let kind = if criterion["required"] == json!(false) {
        " (optional)"
    } else if criterion["human_judgment"] == json!(true) {
        " (judgment, not a check)"
    } else {
        ""
    };
    format!(
        "    [{}] {}{kind}\n        {}\n",
        criterion["state"].as_str().unwrap_or("?"),
        criterion["statement"].as_str().unwrap_or(""),
        criterion["why"].as_str().unwrap_or(""),
    )
}
