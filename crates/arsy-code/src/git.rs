use crate::resource::Workspace;
use arsy_kernel::{
    artifact::{ArtifactStore, NewArtifact, Sensitivity},
    capability::{CapabilityAction, CapabilityGrant},
    domain::{Principal, ResourceRef},
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
    policy::WorkspaceCleanliness,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
};

const MAX_GIT_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_LOG_ENTRIES: u64 = 1_000;

/// Whether the working tree has uncommitted changes, or `None` when that
/// cannot be established — Git is absent, or this is not a repository.
///
/// Policy raises a Git mutation to approval in a dirty tree, so a caller that
/// wants a truthful decision needs this before it evaluates. It is a read-only
/// `git status`, which is why it does not go through the operation registry:
/// asking for a grant in order to explain a grant would not terminate.
pub fn cleanliness(workspace: &Path) -> Option<WorkspaceCleanliness> {
    let output = Command::new("git")
        .args(["--no-pager", "status", "--porcelain"])
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(if output.stdout.is_empty() {
        WorkspaceCleanliness::Clean
    } else {
        WorkspaceCleanliness::Dirty
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitOperation {
    Status,
    Diff,
    Log,
    Blame,
}

impl GitOperation {
    const fn kind(self) -> &'static str {
        match self {
            Self::Status => "git.status",
            Self::Diff => "git.diff",
            Self::Log => "git.log",
            Self::Blame => "git.blame",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffInput {
    revision: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogInput {
    max_entries: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlameInput {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitResult {
    pub operation: String,
    pub output: String,
    pub workspace: Option<WorkspaceCleanliness>,
}

pub struct GitExecutor {
    operation: GitOperation,
    contract: OperationContract,
    workspace: PathBuf,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
}

impl GitExecutor {
    pub fn new(
        operation: GitOperation,
        workspace: &Workspace,
        artifacts: Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
    ) -> Arc<Self> {
        let required = match operation {
            GitOperation::Status => BTreeMap::new(),
            GitOperation::Diff => BTreeMap::from([("revision".into(), JsonType::String)]),
            GitOperation::Log => BTreeMap::from([("max_entries".into(), JsonType::Number)]),
            GitOperation::Blame => BTreeMap::from([("path".into(), JsonType::String)]),
        };
        Arc::new(Self {
            operation,
            contract: OperationContract {
                kind: OperationKind::new(operation.kind()).expect("static operation kind is valid"),
                input_schema: InputSchema {
                    required,
                    allow_extra: false,
                },
                actions: vec![CapabilityAction::GitRead],
                idempotency: Idempotency::Idempotent,
                concurrency: ConcurrencyRule::Parallel,
            },
            workspace: workspace.path().to_owned(),
            artifacts,
            retain_until_ms,
        })
    }

    fn arguments(&self, input: &serde_json::Value) -> Result<Vec<String>, OperationError> {
        match self.operation {
            GitOperation::Status => {
                serde_json::from_value::<EmptyInput>(input.clone()).map_err(schema)?;
                Ok(vec!["status".into(), "--porcelain=v1".into()])
            }
            GitOperation::Diff => {
                let input: DiffInput = serde_json::from_value(input.clone()).map_err(schema)?;
                if input.revision.is_empty()
                    || input.revision.len() > 256
                    || input.revision.starts_with('-')
                    || !input
                        .revision
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"/._~^{}".contains(&byte))
                {
                    return Err(OperationError::Schema("invalid Git revision".into()));
                }
                Ok(vec![
                    "diff".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                    input.revision,
                    "--".into(),
                ])
            }
            GitOperation::Log => {
                let input: LogInput = serde_json::from_value(input.clone()).map_err(schema)?;
                if !(1..=MAX_LOG_ENTRIES).contains(&input.max_entries) {
                    return Err(OperationError::Schema(
                        "max_entries must be between 1 and 1000".into(),
                    ));
                }
                Ok(vec![
                    "log".into(),
                    format!("--max-count={}", input.max_entries),
                    "--format=%H%x09%an%x09%aI%x09%s".into(),
                ])
            }
            GitOperation::Blame => {
                let input: BlameInput = serde_json::from_value(input.clone()).map_err(schema)?;
                let path = confined(&input.path)?;
                Ok(vec![
                    "blame".into(),
                    "--line-porcelain".into(),
                    "--".into(),
                    path,
                ])
            }
        }
    }

    fn run(
        &self,
        input: &serde_json::Value,
        actor: &Principal,
    ) -> Result<OperationOutcome, OperationError> {
        let args = self.arguments(input)?;
        let mut child = Command::new("git")
            .args([
                "--no-pager",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.quotepath=true",
            ])
            .args(&args)
            .current_dir(&self.workspace)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(execution)?;
        let stdout = drain(child.stdout.take().expect("stdout is piped"));
        let stderr = drain(child.stderr.take().expect("stderr is piped"));
        let status = child.wait().map_err(execution)?;
        let stdout = stdout.join().map_err(|_| panicked())?.map_err(execution)?;
        let stderr = stderr.join().map_err(|_| panicked())?.map_err(execution)?;
        if !status.success() {
            return Err(OperationError::Execution(
                String::from_utf8_lossy(&stderr).into_owned(),
            ));
        }
        let output = String::from_utf8(stdout)
            .map_err(|_| OperationError::Execution("Git output is not UTF-8".into()))?;
        let result = GitResult {
            operation: self.operation.kind().into(),
            workspace: (self.operation == GitOperation::Status).then_some(if output.is_empty() {
                WorkspaceCleanliness::Clean
            } else {
                WorkspaceCleanliness::Dirty
            }),
            output,
        };
        let metadata = self
            .artifacts
            .put(
                &serde_json::to_vec(&result)
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
                NewArtifact {
                    media_type: "application/json".into(),
                    creator: actor.clone(),
                    source_revision: None,
                    sensitivity: Sensitivity::Internal,
                    retain_until_ms: self.retain_until_ms,
                },
            )
            .map_err(|error| OperationError::Execution(error.to_string()))?;
        let evidence = metadata.resource_ref();
        Ok(OperationOutcome {
            value: Some(evidence.clone()),
            observed_effects: vec![Effect {
                action: CapabilityAction::GitRead,
                resource: ResourceRef::new("git", ".")
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
            }],
            evidence: vec![evidence],
            state: None,
        })
    }
}

impl OperationExecutor for GitExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        if !request
            .requirements
            .iter()
            .any(|requirement| requirement.action == CapabilityAction::GitRead)
        {
            return Err(OperationError::Schema(
                "Git read operations require a git.read capability".into(),
            ));
        }
        self.run(&request.input, &request.actor)
    }
}

fn confined(path: &str) -> Result<String, OperationError> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(OperationError::Schema(
            "path must stay within the workspace".into(),
        ));
    }
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| OperationError::Schema("path must be UTF-8".into()))
}

fn drain(stream: impl Read + Send + 'static) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut stream = stream;
        let mut bytes = Vec::new();
        let mut overflow = false;
        let mut chunk = [0; 8192];
        loop {
            let count = stream.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            let remaining = MAX_GIT_OUTPUT_BYTES.saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..count.min(remaining)]);
            overflow |= count > remaining;
        }
        if overflow {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "Git output exceeds 1048576 bytes",
            ));
        }
        Ok(bytes)
    })
}

fn schema(error: serde_json::Error) -> OperationError {
    OperationError::Schema(error.to_string())
}

fn execution(error: io::Error) -> OperationError {
    OperationError::Execution(error.to_string())
}

fn panicked() -> OperationError {
    OperationError::Execution("Git output reader panicked".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        artifact::{ArtifactReadLimits, FileArtifactStore},
        domain::{ArtifactId, OperationId},
    };
    use serde_json::json;
    use std::{process::Command, str::FromStr};

    #[test]
    fn typed_reads_report_cleanliness_and_attach_evidence() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "Test"]);
        std::fs::write(temp.path().join("tracked.txt"), "first\n").unwrap();
        git(temp.path(), &["add", "tracked.txt"]);
        git(temp.path(), &["commit", "-qm", "initial"]);
        std::fs::write(temp.path().join("tracked.txt"), "second\n").unwrap();

        let workspace = Workspace::open(temp.path()).unwrap();
        let artifacts =
            Arc::new(FileArtifactStore::open(temp.path().join("artifacts"), 0).unwrap());
        let cases = [
            (GitOperation::Status, json!({})),
            (GitOperation::Diff, json!({"revision": "HEAD"})),
            (GitOperation::Log, json!({"max_entries": 1})),
            (GitOperation::Blame, json!({"path": "tracked.txt"})),
        ];

        for (operation, input) in cases {
            let executor = GitExecutor::new(operation, &workspace, artifacts.clone(), 0);
            assert_eq!(executor.contract().actions, [CapabilityAction::GitRead]);
            let outcome = executor
                .execute(
                    &OperationRequest {
                        id: OperationId::new(),
                        kind: executor.contract().kind.clone(),
                        actor: Principal::System,
                        requirements: vec![arsy_kernel::capability::CapabilityRequirement::new(
                            CapabilityAction::GitRead,
                            ResourceRef::new("git", ".").unwrap(),
                        )],
                        input,
                    },
                    &[],
                )
                .unwrap();
            assert_eq!(outcome.value.as_ref(), outcome.evidence.first());
            let id = ArtifactId::from_str(outcome.evidence[0].value()).unwrap();
            let bytes = artifacts
                .read(
                    id,
                    ArtifactReadLimits {
                        max_bytes: MAX_GIT_OUTPUT_BYTES as u64,
                        max_expansion_ratio: 100,
                    },
                )
                .unwrap();
            let result: GitResult = serde_json::from_slice(&bytes).unwrap();
            assert!(!result.output.is_empty());
            if operation == GitOperation::Status {
                assert_eq!(result.workspace, Some(WorkspaceCleanliness::Dirty));
            }
        }

        assert!(
            GitExecutor::new(GitOperation::Blame, &workspace, artifacts, 0)
                .arguments(&json!({"path": "../outside"}))
                .is_err()
        );
    }

    fn git(path: &Path, args: &[&str]) {
        assert!(Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .unwrap()
            .success());
    }
}
