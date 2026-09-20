//! Whether work may be called verified, and what says so.
//!
//! # The claim this module refuses
//!
//! "The model finished" and "the work is verified" are different sentences,
//! and a harness that lets the first stand in for the second is not an
//! engineering tool. A provider returning a turn, a process exiting zero, a
//! summary saying the tests pass — none of those is evidence about acceptance
//! criteria. This module is what stands between them.
//!
//! # What a proof is
//!
//! A [`CompletionProof`] is rebuilt, never stored-and-trusted. Building one
//! reads the criteria a task committed to, the checks its session recorded,
//! the verdicts people gave, and the artifacts those cite, and decides each
//! criterion for itself against one workspace revision. Anything a reader
//! would have to take on faith is checked here instead:
//!
//! - an artifact id that names nothing in the store is not evidence;
//! - a pass from a different command is not a pass for this criterion;
//! - a pass recorded against another task is not about this one;
//! - a pass against another revision is stale, whatever it said;
//! - a pass from an attempt that was superseded or expired cannot commit;
//! - a model's own assertion is not admitted at all, because nothing a model
//!   says reaches this module — only recorded operations do.
//!
//! The manifest can be written to the artifact store so a later reader has
//! something to compare against, but the verdict never comes from the stored
//! copy: `arsy verify` rebuilds it and says so if the two disagree.

use crate::{
    artifact::ArtifactStore,
    domain::{
        ArtifactId, AttemptId, CriterionId, Principal, SessionId, StateVersion, TaskId,
        WorkspaceVersion,
    },
    orchestration::{AcceptanceCriterion, AttemptState, TaskGraph, Verifier},
    validation::{ValidationOutcome, ValidationRecord},
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Wire version of [`CompletionProof`].
pub const PROOF_SCHEMA_VERSION: u32 = 1;

/// The media type a written manifest carries in the artifact store.
pub const PROOF_MEDIA_TYPE: &str = "application/vnd.arsy.completion-proof+json";

/// Where one criterion, or one task, stands.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofState {
    /// Nothing has been offered for it.
    Unverified,
    /// Some required criteria are met and others are not.
    PartiallyVerified,
    /// Evidence exists but is about another revision.
    Stale,
    /// Evidence exists and says it does not hold.
    Failed,
    /// Nothing available can decide it.
    Unverifiable,
    /// Met, with valid fresh evidence.
    Verified,
}

impl ProofState {
    /// Whether this state lets a task be called verified.
    pub const fn is_verified(self) -> bool {
        matches!(self, Self::Verified)
    }
}

/// One thing the proof points at.
///
/// Every field a reader would otherwise have to assume: which operation, in
/// which task and attempt, by whom, against which revision, and when.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvidenceRef {
    pub kind: String,
    pub artifact: Option<ArtifactId>,
    pub command: Option<String>,
    pub command_digest: Option<StateVersion>,
    pub task: Option<TaskId>,
    pub attempt: Option<AttemptId>,
    pub actor: Principal,
    pub workspace_revision: Option<WorkspaceVersion>,
    pub recorded_at_ms: u64,
    /// Present when the evidence was rejected, saying which check rejected it.
    pub rejected: Option<String>,
}

/// One criterion, decided.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CriterionProof {
    pub criterion: CriterionId,
    pub statement: String,
    pub required: bool,
    pub state: ProofState,
    /// Why it is in that state, in one sentence a person can act on.
    pub why: String,
    /// Whether a person, rather than a check, decided it.
    pub human_judgment: bool,
    pub evidence: Vec<EvidenceRef>,
}

/// What a task's verification rests on, as one document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompletionProof {
    pub schema: u32,
    pub session: SessionId,
    pub task: TaskId,
    /// The revision the proof answers for. Evidence about any other revision
    /// is stale here, whatever it said there.
    pub revision: Option<WorkspaceVersion>,
    pub state: ProofState,
    pub criteria: Vec<CriterionProof>,
    pub built_at_ms: u64,
}

impl CompletionProof {
    /// The criteria a caller has to act on, worst first.
    pub fn outstanding(&self) -> Vec<&CriterionProof> {
        let mut outstanding: Vec<&CriterionProof> = self
            .criteria
            .iter()
            .filter(|criterion| criterion.required && !criterion.state.is_verified())
            .collect();
        outstanding.sort_by_key(|criterion| criterion.state);
        outstanding
    }

    /// What a command-line caller should exit with.
    ///
    /// Zero only for verified. Everything else is a different reason to look,
    /// and collapsing them would make a stale proof indistinguishable from a
    /// failing one in CI.
    pub const fn exit_code(&self) -> i32 {
        match self.state {
            ProofState::Verified => 0,
            ProofState::Failed => 1,
            ProofState::Stale => 2,
            ProofState::Unverifiable => 3,
            ProofState::Unverified | ProofState::PartiallyVerified => 4,
        }
    }
}

/// Builds a proof from what the session actually recorded.
pub struct Prover<'a> {
    graph: &'a TaskGraph,
    validations: &'a [ValidationRecord],
    artifacts: Option<&'a dyn ArtifactStore>,
}

impl<'a> Prover<'a> {
    /// `artifacts` is what makes a forged or deleted artifact id detectable.
    /// Passing `None` is allowed for a caller with no store to check against,
    /// and every criterion that would have cited one is reported unverifiable
    /// rather than quietly accepted.
    pub fn new(
        graph: &'a TaskGraph,
        validations: &'a [ValidationRecord],
        artifacts: Option<&'a dyn ArtifactStore>,
    ) -> Self {
        Self {
            graph,
            validations,
            artifacts,
        }
    }

    /// Decide every criterion of one task against one revision.
    pub fn prove(
        &self,
        task: TaskId,
        revision: Option<WorkspaceVersion>,
        now_ms: u64,
    ) -> CompletionProof {
        let criteria: Vec<CriterionProof> = self
            .graph
            .criteria_of(task)
            .into_iter()
            .map(|criterion| self.decide(criterion, revision, now_ms))
            .collect();
        CompletionProof {
            schema: PROOF_SCHEMA_VERSION,
            session: self.graph.session(),
            task,
            revision,
            state: overall(&criteria),
            criteria,
            built_at_ms: now_ms,
        }
    }

    fn decide(
        &self,
        criterion: &AcceptanceCriterion,
        revision: Option<WorkspaceVersion>,
        now_ms: u64,
    ) -> CriterionProof {
        let proof = |state: ProofState, why: &str, evidence: Vec<EvidenceRef>| CriterionProof {
            criterion: criterion.id,
            statement: criterion.statement.clone(),
            required: criterion.required,
            state,
            why: why.to_owned(),
            human_judgment: matches!(criterion.verifier, Verifier::Review | Verifier::Human),
            evidence,
        };
        // A criterion that no longer applies is met by not applying, and the
        // reason travels with the proof rather than being dropped.
        if let Some(why) = &criterion.inapplicable {
            return proof(ProofState::Verified, why, Vec::new());
        }
        match &criterion.verifier {
            Verifier::Unverifiable { why } => proof(ProofState::Unverifiable, why, Vec::new()),
            Verifier::Review | Verifier::Human => self.judged(criterion, revision, proof),
            Verifier::Command { digest } => {
                self.checked(criterion, *digest, revision, now_ms, proof)
            }
        }
    }

    /// A criterion a person decides.
    fn judged(
        &self,
        criterion: &AcceptanceCriterion,
        revision: Option<WorkspaceVersion>,
        proof: impl Fn(ProofState, &str, Vec<EvidenceRef>) -> CriterionProof,
    ) -> CriterionProof {
        let Some(verdict) = self.graph.judgments_of(criterion.id).last() else {
            return proof(
                ProofState::Unverified,
                "nobody has given a verdict on this",
                Vec::new(),
            );
        };
        let evidence = vec![EvidenceRef {
            kind: "judgment".to_owned(),
            artifact: None,
            command: None,
            command_digest: None,
            task: Some(criterion.task),
            attempt: None,
            actor: verdict.actor.clone(),
            workspace_revision: verdict.workspace_revision,
            recorded_at_ms: verdict.recorded_at_ms,
            rejected: None,
        }];
        if !verdict.met {
            return proof(ProofState::Failed, &verdict.note, evidence);
        }
        if stale(verdict.workspace_revision, revision) {
            return proof(
                ProofState::Stale,
                "the verdict was given about another revision",
                evidence,
            );
        }
        proof(
            ProofState::Verified,
            "a reviewer says it holds; this is a judgment, not a deterministic check",
            evidence,
        )
    }

    /// A criterion a recorded command decides.
    fn checked(
        &self,
        criterion: &AcceptanceCriterion,
        digest: StateVersion,
        revision: Option<WorkspaceVersion>,
        now_ms: u64,
        proof: impl Fn(ProofState, &str, Vec<EvidenceRef>) -> CriterionProof,
    ) -> CriterionProof {
        // Only records of *this* command, so a pass from a different one
        // cannot be offered in its place.
        let candidates: Vec<&ValidationRecord> = self
            .validations
            .iter()
            .filter(|record| record.command_digest == digest)
            .collect();
        let Some(record) = candidates.last().copied() else {
            return proof(
                ProofState::Unverified,
                "this check has not been run",
                Vec::new(),
            );
        };
        let mut reference = EvidenceRef {
            kind: "validation".to_owned(),
            artifact: Some(record.artifact),
            command: Some(record.command.clone()),
            command_digest: Some(record.command_digest),
            task: record.task,
            attempt: record.attempt,
            actor: Principal::System,
            workspace_revision: record.workspace_revision,
            recorded_at_ms: record.recorded_at_ms,
            rejected: None,
        };
        let reject = |why: &str, mut reference: EvidenceRef| {
            reference.rejected = Some(why.to_owned());
            (why.to_owned(), vec![reference])
        };

        if record.task.is_some_and(|task| task != criterion.task) {
            let (why, evidence) = reject("the check belongs to another task", reference);
            return proof(ProofState::Unverified, &why, evidence);
        }
        // An attempt that was superseded or expired cannot commit a result,
        // and evidence it produced cannot satisfy a criterion either.
        if let Some(attempt) = record
            .attempt
            .and_then(|attempt| self.graph.attempt(attempt))
        {
            if matches!(
                attempt.state,
                AttemptState::Superseded | AttemptState::Expired
            ) {
                let (why, evidence) = reject("the attempt that ran it was superseded", reference);
                return proof(ProofState::Unverified, &why, evidence);
            }
        }
        match self.artifacts {
            None => {
                let (why, evidence) =
                    reject("no artifact store to check the evidence against", reference);
                return proof(ProofState::Unverifiable, &why, evidence);
            }
            Some(store) => {
                if store.metadata(record.artifact).is_err() {
                    let (why, evidence) =
                        reject("the artifact this cites is not in the store", reference);
                    return proof(ProofState::Unverified, &why, evidence);
                }
            }
        }
        if record.outcome == ValidationOutcome::Failed {
            reference.rejected = None;
            return proof(ProofState::Failed, &record.detail, vec![reference]);
        }
        if stale(record.workspace_revision, revision) {
            let (why, evidence) = reject("the check ran against another revision", reference);
            return proof(ProofState::Stale, &why, evidence);
        }
        if criterion
            .freshness_ms
            .is_some_and(|window| now_ms.saturating_sub(record.recorded_at_ms) > window)
        {
            let (why, evidence) =
                reject("the check is older than this criterion allows", reference);
            return proof(ProofState::Stale, &why, evidence);
        }
        proof(ProofState::Verified, &record.command, vec![reference])
    }
}

/// Evidence about a revision that is not the one being proved.
///
/// An unknown revision on either side cannot establish a mismatch, and is not
/// treated as one — the same rule the validation log uses, so the two cannot
/// disagree about what "stale" means.
fn stale(recorded: Option<WorkspaceVersion>, current: Option<WorkspaceVersion>) -> bool {
    matches!((recorded, current), (Some(recorded), Some(current)) if recorded != current)
}

/// One state for the whole task.
///
/// A task with no criteria is unverified rather than verified: nothing was
/// asked of it, so nothing was shown.
fn overall(criteria: &[CriterionProof]) -> ProofState {
    let required: Vec<&CriterionProof> = criteria
        .iter()
        .filter(|criterion| criterion.required)
        .collect();
    if required.is_empty() {
        return ProofState::Unverified;
    }
    // Worst wins, and the ordering of `ProofState` is what makes that one
    // comparison rather than a ladder of special cases.
    let worst = required
        .iter()
        .map(|criterion| criterion.state)
        .min()
        .unwrap_or(ProofState::Unverified);
    if worst == ProofState::Unverified
        && required
            .iter()
            .any(|criterion| criterion.state.is_verified())
    {
        return ProofState::PartiallyVerified;
    }
    worst
}

#[derive(Debug)]
pub enum ProofError {
    NotVerified(ProofState),
    /// The stored manifest and the rebuilt one disagree.
    Disagrees {
        stored: ProofState,
        rebuilt: ProofState,
    },
}

impl fmt::Display for ProofError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotVerified(state) => {
                write!(formatter, "the work is {state:?}, not verified")
            }
            Self::Disagrees { stored, rebuilt } => write!(
                formatter,
                "the recorded proof says {stored:?} and rebuilding it says {rebuilt:?}"
            ),
        }
    }
}

impl std::error::Error for ProofError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        artifact::{FileArtifactStore, NewArtifact, Sensitivity},
        domain::{AgentId, SessionId},
        event::{EventStore, MemoryEventStore},
        orchestration::{
            Budget, TaskGraph, TaskNode, TaskRuntime, TaskState, WorkspaceRequirement,
        },
        validation::{command_digest, ValidationRecord, VALIDATION_SCHEMA_VERSION},
    };
    use std::sync::Arc;

    const CHECK: &str = "cargo test --workspace";

    fn graph_with_task() -> (TaskGraph, TaskId) {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let mut graph = TaskGraph::new(store, SessionId::new(), Principal::System).unwrap();
        let id = TaskId::new();
        graph
            .add(TaskNode {
                id,
                goal: "fix the parser".into(),
                dependencies: Vec::new(),
                assignee: None,
                required_output: "evidence".into(),
                workspace: WorkspaceRequirement::ReadOnlySnapshot,
                budget: Budget {
                    tokens: 10,
                    cost_micros: 10,
                    wall_ms: 10,
                },
                authority: Vec::new(),
                state: TaskState::Pending,
                lease_expires_at_ms: None,
                runtime: TaskRuntime::default(),
            })
            .unwrap();
        (graph, id)
    }

    fn criterion(task: TaskId, verifier: Verifier) -> AcceptanceCriterion {
        AcceptanceCriterion {
            id: CriterionId::new(),
            task,
            statement: "the suite passes".into(),
            verifier,
            required: true,
            freshness_ms: None,
            inapplicable: None,
        }
    }

    fn record(
        task: Option<TaskId>,
        command: &str,
        artifact: ArtifactId,
        revision: Option<WorkspaceVersion>,
        outcome: ValidationOutcome,
    ) -> ValidationRecord {
        ValidationRecord {
            schema: VALIDATION_SCHEMA_VERSION,
            sequence: 1,
            operation: "process.run".into(),
            command: command.to_owned(),
            command_digest: command_digest(command),
            artifact,
            task,
            attempt: None,
            grants: Vec::new(),
            workspace_revision: revision,
            outcome,
            detail: "1 failed".into(),
            recorded_at_ms: 1_000,
        }
    }

    /// A store holding one real artifact, and the id of one that never was.
    fn artifacts() -> (tempfile::TempDir, FileArtifactStore, ArtifactId, ArtifactId) {
        let directory = tempfile::tempdir().unwrap();
        let store = FileArtifactStore::open(directory.path(), 0).unwrap();
        let real = store
            .put(
                b"ok",
                NewArtifact {
                    media_type: "text/plain".into(),
                    creator: Principal::System,
                    source_revision: None,
                    sensitivity: Sensitivity::Public,
                    retain_until_ms: u64::MAX,
                },
            )
            .unwrap()
            .id;
        (directory, store, real, ArtifactId::new())
    }

    fn revision(byte: u8) -> WorkspaceVersion {
        WorkspaceVersion(StateVersion::from_digest([byte; 32]))
    }

    #[test]
    fn a_task_nobody_asked_anything_of_is_not_verified() {
        let (graph, task) = graph_with_task();
        let proof = Prover::new(&graph, &[], None).prove(task, None, 0);
        assert_eq!(proof.state, ProofState::Unverified);
        assert_ne!(proof.exit_code(), 0);
    }

    #[test]
    fn a_passing_check_against_this_revision_verifies_its_criterion() {
        let (mut graph, task) = graph_with_task();
        let (_directory, store, real, _) = artifacts();
        graph
            .declare_criterion(criterion(
                task,
                Verifier::Command {
                    digest: command_digest(CHECK),
                },
            ))
            .unwrap();
        let records = vec![record(
            Some(task),
            CHECK,
            real,
            Some(revision(1)),
            ValidationOutcome::Passed,
        )];
        let proof = Prover::new(&graph, &records, Some(&store)).prove(task, Some(revision(1)), 0);
        assert_eq!(proof.state, ProofState::Verified);
        assert_eq!(proof.exit_code(), 0);
        assert!(proof.outstanding().is_empty());
    }

    #[test]
    fn every_way_of_faking_evidence_is_refused() {
        let (mut graph, task) = graph_with_task();
        let (_directory, store, real, missing) = artifacts();
        let declared = graph
            .declare_criterion(criterion(
                task,
                Verifier::Command {
                    digest: command_digest(CHECK),
                },
            ))
            .unwrap();
        let prove = |records: &[ValidationRecord]| {
            Prover::new(&graph, records, Some(&store))
                .prove(task, Some(revision(1)), 0)
                .state
        };

        // A pass from a different command is not this criterion's evidence.
        assert_eq!(
            prove(&[record(
                Some(task),
                "cargo test --lib parser",
                real,
                Some(revision(1)),
                ValidationOutcome::Passed
            )]),
            ProofState::Unverified
        );
        // A pass recorded against another task.
        assert_eq!(
            prove(&[record(
                Some(TaskId::new()),
                CHECK,
                real,
                Some(revision(1)),
                ValidationOutcome::Passed
            )]),
            ProofState::Unverified
        );
        // An artifact id that names nothing: forged, or since deleted.
        assert_eq!(
            prove(&[record(
                Some(task),
                CHECK,
                missing,
                Some(revision(1)),
                ValidationOutcome::Passed
            )]),
            ProofState::Unverified
        );
        // A pass about a revision the workspace has left — which is also what
        // an edit made after the check looks like, because the revision
        // digest covers the working tree.
        assert_eq!(
            prove(&[record(
                Some(task),
                CHECK,
                real,
                Some(revision(2)),
                ValidationOutcome::Passed
            )]),
            ProofState::Stale
        );
        // And a failure is a failure, whatever anyone says about it.
        assert_eq!(
            prove(&[record(
                Some(task),
                CHECK,
                real,
                Some(revision(1)),
                ValidationOutcome::Failed
            )]),
            ProofState::Failed
        );

        // Each refusal says which check rejected the evidence.
        let proof = Prover::new(
            &graph,
            &[record(
                Some(task),
                CHECK,
                missing,
                Some(revision(1)),
                ValidationOutcome::Passed,
            )],
            Some(&store),
        )
        .prove(task, Some(revision(1)), 0);
        let decided = &proof.criteria[0];
        assert_eq!(decided.criterion, declared);
        assert_eq!(
            decided.evidence[0].rejected.as_deref(),
            Some("the artifact this cites is not in the store")
        );
    }

    #[test]
    fn evidence_from_a_superseded_attempt_cannot_satisfy_anything() {
        let (mut graph, task) = graph_with_task();
        let (_directory, store, real, _) = artifacts();
        graph
            .declare_criterion(criterion(
                task,
                Verifier::Command {
                    digest: command_digest(CHECK),
                },
            ))
            .unwrap();
        graph.ready().unwrap();
        let attempt = graph.lease(task, AgentId::new(), 1_000).unwrap();
        graph
            .finish_attempt(
                attempt,
                &crate::orchestration::AttemptOutcome::failed(Budget::default(), "gave up", 1)
                    .retryable(crate::orchestration::Retryability::Retryable),
            )
            .unwrap();
        graph
            .retry(
                task,
                &crate::orchestration::AttemptRequest {
                    role: "retry".into(),
                    assignee: AgentId::new(),
                    model: None,
                    base_revision: None,
                    started_at_ms: 2,
                    lease_expires_at_ms: 1_000,
                },
                3,
            )
            .unwrap();

        let records = vec![ValidationRecord {
            attempt: Some(attempt),
            ..record(
                Some(task),
                CHECK,
                real,
                Some(revision(1)),
                ValidationOutcome::Passed,
            )
        }];
        let proof = Prover::new(&graph, &records, Some(&store)).prove(task, Some(revision(1)), 0);
        assert_eq!(proof.state, ProofState::Unverified);
        assert_eq!(
            proof.criteria[0].evidence[0].rejected.as_deref(),
            Some("the attempt that ran it was superseded")
        );
    }

    #[test]
    fn a_human_criterion_is_verified_by_a_verdict_and_labelled_as_one() {
        let (mut graph, task) = graph_with_task();
        let declared = graph
            .declare_criterion(criterion(task, Verifier::Human))
            .unwrap();

        // Nobody has looked yet.
        let proof = Prover::new(&graph, &[], None).prove(task, Some(revision(1)), 0);
        assert_eq!(proof.state, ProofState::Unverified);

        graph
            .judge(crate::orchestration::Judgment {
                criterion: declared,
                actor: Principal::User("dev".into()),
                met: true,
                note: "read it; the error message is right".into(),
                workspace_revision: Some(revision(1)),
                recorded_at_ms: 5,
            })
            .unwrap();
        let proof = Prover::new(&graph, &[], None).prove(task, Some(revision(1)), 0);
        assert_eq!(proof.state, ProofState::Verified);
        assert!(proof.criteria[0].human_judgment);
        assert_eq!(
            proof.criteria[0].evidence[0].actor,
            Principal::User("dev".into())
        );
        assert!(
            proof.criteria[0].why.contains("not a deterministic check"),
            "{}",
            proof.criteria[0].why
        );
    }

    #[test]
    fn an_optional_or_retired_criterion_does_not_block_and_stays_visible() {
        let (mut graph, task) = graph_with_task();
        let (_directory, store, real, _) = artifacts();
        graph
            .declare_criterion(criterion(
                task,
                Verifier::Command {
                    digest: command_digest(CHECK),
                },
            ))
            .unwrap();
        let optional = graph
            .declare_criterion(AcceptanceCriterion {
                required: false,
                statement: "the benchmark did not regress".into(),
                ..criterion(
                    task,
                    Verifier::Command {
                        digest: command_digest("cargo bench"),
                    },
                )
            })
            .unwrap();
        let removed = graph
            .declare_criterion(AcceptanceCriterion {
                statement: "the old parser still works".into(),
                ..criterion(
                    task,
                    Verifier::Command {
                        digest: command_digest("cargo test --lib old_parser"),
                    },
                )
            })
            .unwrap();
        graph
            .retire_criterion(removed, "the old parser was deleted by this change")
            .unwrap();

        let records = vec![record(
            Some(task),
            CHECK,
            real,
            Some(revision(1)),
            ValidationOutcome::Passed,
        )];
        let proof = Prover::new(&graph, &records, Some(&store)).prove(task, Some(revision(1)), 0);
        assert_eq!(proof.state, ProofState::Verified);
        // Both stay in the document: what was not required and what stopped
        // applying are things a reader is entitled to see.
        assert_eq!(proof.criteria.len(), 3);
        assert!(proof
            .criteria
            .iter()
            .any(|decided| decided.criterion == optional && !decided.required));
        assert!(proof
            .criteria
            .iter()
            .any(|decided| decided.criterion == removed
                && decided.why.contains("deleted by this change")));
    }

    #[test]
    fn one_unmet_required_criterion_keeps_the_task_short_of_verified() {
        let (mut graph, task) = graph_with_task();
        let (_directory, store, real, _) = artifacts();
        graph
            .declare_criterion(criterion(
                task,
                Verifier::Command {
                    digest: command_digest(CHECK),
                },
            ))
            .unwrap();
        graph
            .declare_criterion(AcceptanceCriterion {
                statement: "someone reviewed it".into(),
                ..criterion(task, Verifier::Review)
            })
            .unwrap();
        let records = vec![record(
            Some(task),
            CHECK,
            real,
            Some(revision(1)),
            ValidationOutcome::Passed,
        )];
        let proof = Prover::new(&graph, &records, Some(&store)).prove(task, Some(revision(1)), 0);
        assert_eq!(proof.state, ProofState::PartiallyVerified);
        assert_eq!(proof.outstanding().len(), 1);
        assert_ne!(proof.exit_code(), 0);
    }
}
