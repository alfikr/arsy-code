//! `validate.*`: the edit → validate → fix loop, as explicit agent progress.
//!
//! A model that runs `pytest` through `process.exec` and reads the output is
//! validating, but nothing durable says so: the transcript can be trimmed,
//! and "did this pass before I claimed done" becomes a question only the
//! model's memory can answer. `validate.record` makes one run of a check a
//! fact the task carries — recorded against it, not just typed at it — so
//! `validate.status` can answer "is the last known validation state a pass"
//! without re-reading the transcript, and a completion claim can cite it.

use crate::process::ProcessResult;
use arsy_kernel::{
    artifact::{ArtifactReadLimits, ArtifactStore},
    capability::{CapabilityAction, CapabilityGrant},
    domain::{ArtifactId, Principal, ResourceRef},
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

/// How much of an evidence artifact is read to find its exit code. Well past
/// any real `process.exec` result, which is a handful of fields.
const EVIDENCE_LIMITS: ArtifactReadLimits = ArtifactReadLimits {
    max_bytes: 64 * 1024,
    max_expansion_ratio: 1_000,
};

/// Longest excerpt of a check's own output kept against the record. Enough to
/// show the failing assertion, little enough that a noisy test runner cannot
/// crowd out the plan it is validating.
const MAX_DETAIL_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationOutcome {
    Passed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValidationRecord {
    pub sequence: u64,
    pub command: String,
    pub outcome: ValidationOutcome,
    /// What the check itself said, truncated. Empty when the caller has
    /// nothing more to add than pass or fail.
    pub detail: String,
    pub recorded_at_ms: u64,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValidationLog {
    pub records: Vec<ValidationRecord>,
}

impl ValidationLog {
    /// Whether the task this log belongs to is in an actionable state: no
    /// check has run yet, or the most recent one failed. `false` once the
    /// last recorded check passed, which is what a completion claim cites.
    pub fn actionable(&self) -> bool {
        !matches!(
            self.records.last().map(|record| record.outcome),
            Some(ValidationOutcome::Passed)
        )
    }
}

#[derive(Default)]
pub struct ValidationState {
    records: Vec<ValidationRecord>,
}

/// A fresh, empty log, with no lifetime beyond whoever holds the `Arc`. Used
/// by tests, which want a log isolated to one case.
pub fn state() -> Arc<Mutex<ValidationState>> {
    Arc::new(Mutex::new(ValidationState::default()))
}

/// Every workspace's validation log, kept alive for the life of the process —
/// the same reason, and the same restart ceiling, as
/// [`planops::state_for`](super::planops::state_for).
static WORKSPACES: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<ValidationState>>>>> = OnceLock::new();

pub fn state_for(workspace_root: &Path) -> Arc<Mutex<ValidationState>> {
    let workspaces = WORKSPACES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut workspaces = workspaces.lock().unwrap_or_else(|error| error.into_inner());
    workspaces
        .entry(workspace_root.to_path_buf())
        .or_insert_with(state)
        .clone()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidateOperation {
    Record,
    Status,
}

impl ValidateOperation {
    pub const ALL: [Self; 2] = [Self::Record, Self::Status];

    const fn kind(self) -> &'static str {
        match self {
            Self::Record => "validate.record",
            Self::Status => "validate.status",
        }
    }

    const fn idempotency(self) -> Idempotency {
        match self {
            Self::Record => Idempotency::Effectful,
            Self::Status => Idempotency::Idempotent,
        }
    }

    /// A read cannot conflict with another read; recording an outcome
    /// mutates the log and has to see what a previous record left.
    const fn concurrency(self) -> ConcurrencyRule {
        match self {
            Self::Status => ConcurrencyRule::Parallel,
            Self::Record => ConcurrencyRule::ExclusiveGlobal,
        }
    }

    fn schema(self) -> InputSchema {
        let string = |name: &str| (name.to_owned(), JsonType::String);
        let (required, optional) = match self {
            Self::Record => (
                vec![string("command"), string("evidence")],
                vec![string("detail")],
            ),
            Self::Status => (Vec::new(), Vec::new()),
        };
        InputSchema {
            required: required.into_iter().collect(),
            optional: optional.into_iter().collect(),
            allow_extra: false,
        }
    }
}

pub struct ValidateExecutor {
    operation: ValidateOperation,
    contract: OperationContract,
    state: Arc<Mutex<ValidationState>>,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
}

impl ValidateExecutor {
    pub fn executors(
        state: &Arc<Mutex<ValidationState>>,
        artifacts: &Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
    ) -> Vec<Arc<dyn OperationExecutor>> {
        ValidateOperation::ALL
            .into_iter()
            .map(|operation| {
                Arc::new(Self {
                    operation,
                    contract: OperationContract {
                        kind: OperationKind::new(operation.kind())
                            .expect("static operation kind is valid"),
                        input_schema: operation.schema(),
                        actions: vec![CapabilityAction::SystemModify],
                        idempotency: operation.idempotency(),
                        reversible: true,
                        concurrency: operation.concurrency(),
                    },
                    state: Arc::clone(state),
                    artifacts: Arc::clone(artifacts),
                    retain_until_ms,
                }) as Arc<dyn OperationExecutor>
            })
            .collect()
    }

    fn put(
        &self,
        value: &impl Serialize,
        creator: Principal,
    ) -> Result<ResourceRef, OperationError> {
        super::store(
            self.artifacts.as_ref(),
            value,
            creator,
            self.retain_until_ms,
        )
    }

    /// The outcome a `bash` call actually had, read from the artifact its own
    /// call returned — not asserted by whoever is calling `validate.record`.
    /// A model that never ran the command, or ran a different one, has no
    /// artifact id to give; one that ran it and got a failure cannot report
    /// `passed` by simply saying so.
    fn outcome_of(&self, evidence: &str) -> Result<ValidationOutcome, OperationError> {
        let id: ArtifactId = evidence
            .parse()
            .map_err(|_| OperationError::Execution("`evidence` is not an artifact id".into()))?;
        let bytes = self
            .artifacts
            .read(id, EVIDENCE_LIMITS)
            .map_err(|error| OperationError::Execution(format!("evidence artifact: {error}")))?;
        let result: ProcessResult = serde_json::from_slice(&bytes).map_err(|_| {
            OperationError::Execution(
                "evidence must be the artifact id a `bash` call returned".into(),
            )
        })?;
        Ok(if !result.timed_out && result.status_code == Some(0) {
            ValidationOutcome::Passed
        } else {
            ValidationOutcome::Failed
        })
    }
}

impl OperationExecutor for ValidateExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let input = &request.input;
        let mut state = self
            .state
            .lock()
            .map_err(|_| OperationError::Execution("validation state poisoned".into()))?;

        if self.operation == ValidateOperation::Record {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if command.is_empty() {
                return Err(OperationError::Execution(
                    "a validation record needs the command that was run".into(),
                ));
            }
            let outcome = self.outcome_of(
                input
                    .get("evidence")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )?;
            let mut detail = input
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            detail.truncate(MAX_DETAIL_BYTES);
            let sequence = state.records.len() as u64 + 1;
            state.records.push(ValidationRecord {
                sequence,
                command: command.to_owned(),
                outcome,
                detail,
                recorded_at_ms: arsy_kernel::artifact::unix_time_ms(),
            });
        }

        let log = ValidationLog {
            records: state.records.clone(),
        };
        drop(state);
        let value = self.put(&log, request.actor.clone())?;

        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: CapabilityAction::SystemModify,
                resource: ResourceRef::new("system", "validation")
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
            }],
            evidence: Vec::new(),
            state: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        artifact::FileArtifactStore,
        domain::{OperationId, Principal},
    };

    fn setup() -> (
        tempfile::TempDir,
        Vec<Arc<dyn OperationExecutor>>,
        Arc<dyn ArtifactStore>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(dir.path().join("artifacts"), 0).unwrap());
        let executors = ValidateExecutor::executors(&state(), &artifacts, 0);
        (dir, executors, artifacts)
    }

    /// The artifact a `bash` call itself would have left, so a test can hand
    /// `validate.record` a real id rather than a claim.
    fn evidence(artifacts: &Arc<dyn ArtifactStore>, status_code: Option<i32>) -> String {
        let result = ProcessResult {
            status_code,
            timed_out: false,
            graceful_termination_sent: false,
            forced_kill_sent: false,
            cleanup: crate::process::Cleanup::Reaped,
            stdout_truncated: false,
            stderr_truncated: false,
            sandbox_assurance: arsy_kernel::policy::SandboxAssurance::None,
        };
        let reference = super::super::store(
            artifacts.as_ref(),
            &result,
            Principal::User("test".into()),
            0,
        )
        .unwrap();
        reference.value().to_owned()
    }

    fn call(
        executors: &[Arc<dyn OperationExecutor>],
        artifacts: &Arc<dyn ArtifactStore>,
        kind: &str,
        input: Value,
    ) -> ValidationLog {
        let executor = executors
            .iter()
            .find(|executor| executor.contract().kind.as_str() == kind)
            .unwrap_or_else(|| panic!("no executor for {kind}"));
        let request = OperationRequest {
            id: OperationId::new(),
            kind: executor.contract().kind.clone(),
            actor: Principal::User("test".into()),
            requirements: Vec::new(),
            input,
        };
        let outcome = executor.execute(&request, &[]).unwrap();
        let reference = outcome.value.expect("validate calls always return a log");
        let id: ArtifactId = reference.value().parse().unwrap();
        let bytes = artifacts
            .read(
                id,
                ArtifactReadLimits {
                    max_bytes: 1024 * 1024,
                    max_expansion_ratio: 1_000,
                },
            )
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn a_failing_run_leaves_the_task_actionable_and_a_passing_one_does_not() {
        let (_dir, executors, artifacts) = setup();

        let log = call(
            &executors,
            &artifacts,
            "validate.record",
            serde_json::json!({
                "command": "cargo test -p arsy-code",
                "evidence": evidence(&artifacts, Some(1)),
                "detail": "assertion failed: left == right"
            }),
        );
        assert_eq!(log.records.len(), 1);
        assert!(log.actionable(), "a failing check leaves work to do");

        let log = call(
            &executors,
            &artifacts,
            "validate.record",
            serde_json::json!({
                "command": "cargo test -p arsy-code",
                "evidence": evidence(&artifacts, Some(0)),
            }),
        );
        assert_eq!(log.records.len(), 2);
        assert!(
            !log.actionable(),
            "the most recent, passing run is what completion cites"
        );

        let status = call(
            &executors,
            &artifacts,
            "validate.status",
            serde_json::json!({}),
        );
        assert_eq!(status, log, "status reads the same log without mutating it");
    }

    /// A model cannot claim a passing check that never ran: there is no
    /// evidence artifact for a command it did not call `bash` for, and an id
    /// it makes up does not resolve.
    #[test]
    fn a_claim_with_no_real_evidence_artifact_is_rejected() {
        let (_dir, executors, _artifacts) = setup();
        let executor = executors
            .iter()
            .find(|executor| executor.contract().kind.as_str() == "validate.record")
            .unwrap();
        let request = OperationRequest {
            id: OperationId::new(),
            kind: executor.contract().kind.clone(),
            actor: Principal::User("test".into()),
            requirements: Vec::new(),
            input: serde_json::json!({"command": "make test", "evidence": "not-an-artifact-id"}),
        };
        assert!(executor.execute(&request, &[]).is_err());
    }

    /// Evidence that resolves but is not a `bash` result — someone else's
    /// artifact, or the plan snapshot from `plan.add` — is rejected the same
    /// way: a real id is necessary but has to name the right kind of thing.
    #[test]
    fn evidence_that_is_not_a_command_result_is_rejected() {
        let (_dir, executors, artifacts) = setup();
        let executor = executors
            .iter()
            .find(|executor| executor.contract().kind.as_str() == "validate.record")
            .unwrap();
        let not_a_process_result = super::super::store(
            artifacts.as_ref(),
            &serde_json::json!({"steps": []}),
            Principal::User("test".into()),
            0,
        )
        .unwrap();
        let request = OperationRequest {
            id: OperationId::new(),
            kind: executor.contract().kind.clone(),
            actor: Principal::User("test".into()),
            requirements: Vec::new(),
            input: serde_json::json!({
                "command": "make test",
                "evidence": not_a_process_result.value(),
            }),
        };
        assert!(executor.execute(&request, &[]).is_err());
    }

    /// A command that timed out is not a pass, whatever its exit code reads
    /// as by the time the process was killed.
    #[test]
    fn a_timed_out_run_is_never_a_pass() {
        let (_dir, executors, artifacts) = setup();
        let timed_out = ProcessResult {
            status_code: Some(0),
            timed_out: true,
            graceful_termination_sent: true,
            forced_kill_sent: false,
            cleanup: crate::process::Cleanup::Terminated,
            stdout_truncated: false,
            stderr_truncated: false,
            sandbox_assurance: arsy_kernel::policy::SandboxAssurance::None,
        };
        let reference = super::super::store(
            artifacts.as_ref(),
            &timed_out,
            Principal::User("test".into()),
            0,
        )
        .unwrap();
        let log = call(
            &executors,
            &artifacts,
            "validate.record",
            serde_json::json!({"command": "sleep 300", "evidence": reference.value()}),
        );
        assert!(
            log.actionable(),
            "a timed-out run is not evidence of a pass"
        );
    }
}
