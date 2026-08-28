use arsy_kernel::{
    artifact::{ArtifactStore, NewArtifact, Sensitivity},
    capability::{CapabilityAction, CapabilityGrant},
    domain::Principal,
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read},
    process::{Child, Command, ExitStatus, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;

#[derive(Debug, Deserialize)]
struct ProcessInput {
    argv: Vec<String>,
    timeout_ms: u64,
    max_output_bytes: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cleanup {
    Reaped,
    Terminated,
    Killed,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessResult {
    pub status_code: Option<i32>,
    pub timed_out: bool,
    pub graceful_termination_sent: bool,
    pub forced_kill_sent: bool,
    pub cleanup: Cleanup,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

pub struct ProcessExecutor {
    contract: OperationContract,
    artifacts: Arc<dyn ArtifactStore>,
    environment: BTreeMap<String, String>,
    grace: Duration,
    retain_until_ms: u64,
}

impl ProcessExecutor {
    pub fn new(
        artifacts: Arc<dyn ArtifactStore>,
        environment_allowlist: impl IntoIterator<Item = String>,
        grace: Duration,
        retain_until_ms: u64,
    ) -> Self {
        let allowed: BTreeSet<_> = environment_allowlist.into_iter().collect();
        let environment = std::env::vars()
            .filter(|(name, _)| allowed.contains(name))
            .collect();
        Self {
            contract: OperationContract {
                kind: OperationKind::new("process.exec").expect("static operation kind is valid"),
                input_schema: InputSchema {
                    required: BTreeMap::from([
                        ("argv".into(), JsonType::Array),
                        ("timeout_ms".into(), JsonType::Number),
                        ("max_output_bytes".into(), JsonType::Number),
                    ]),
                    allow_extra: false,
                },
                actions: vec![CapabilityAction::ProcessExec],
                idempotency: Idempotency::Effectful,
                concurrency: ConcurrencyRule::Parallel,
            },
            artifacts,
            environment,
            grace,
            retain_until_ms,
        }
    }

    fn run(
        &self,
        input: ProcessInput,
        actor: &Principal,
    ) -> Result<OperationOutcome, OperationError> {
        let (program, args) = input
            .argv
            .split_first()
            .ok_or_else(|| OperationError::Schema("argv must not be empty".into()))?;
        if program.is_empty()
            || !(1..=MAX_TIMEOUT_MS).contains(&input.timeout_ms)
            || !(1..=MAX_OUTPUT_BYTES).contains(&input.max_output_bytes)
        {
            return Err(OperationError::Schema(
                "program must be non-empty, timeout_ms at most 86400000, and max_output_bytes at most 16777216".into(),
            ));
        }

        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .envs(&self.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(execution)?;
        let stdout = drain(
            child.stdout.take().expect("piped stdout is present"),
            input.max_output_bytes,
        );
        let stderr = drain(
            child.stderr.take().expect("piped stderr is present"),
            input.max_output_bytes,
        );
        let (status, timed_out, graceful, forced, cleanup) = wait_bounded(
            &mut child,
            Duration::from_millis(input.timeout_ms),
            self.grace,
        )?;
        let stdout = stdout
            .join()
            .map_err(|_| OperationError::Execution("stdout reader panicked".into()))?
            .map_err(execution)?;
        let stderr = stderr
            .join()
            .map_err(|_| OperationError::Execution("stderr reader panicked".into()))?
            .map_err(execution)?;

        let stdout_artifact = self.put(&stdout.bytes, actor.clone(), "application/octet-stream")?;
        let stderr_artifact = self.put(&stderr.bytes, actor.clone(), "application/octet-stream")?;
        let result = ProcessResult {
            status_code: status.code(),
            timed_out,
            graceful_termination_sent: graceful,
            forced_kill_sent: forced,
            cleanup,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
        };
        let result_artifact = self.put(
            &serde_json::to_vec(&result)
                .map_err(|error| OperationError::Execution(error.to_string()))?,
            actor.clone(),
            "application/json",
        )?;

        Ok(OperationOutcome {
            value: Some(result_artifact.clone()),
            observed_effects: vec![Effect {
                action: CapabilityAction::ProcessExec,
                resource: arsy_kernel::domain::ResourceRef::new("process", program.clone())
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
            }],
            evidence: vec![stdout_artifact, stderr_artifact],
            state: None,
        })
    }

    fn put(
        &self,
        bytes: &[u8],
        creator: Principal,
        media_type: &str,
    ) -> Result<arsy_kernel::domain::ResourceRef, OperationError> {
        self.artifacts
            .put(
                bytes,
                NewArtifact {
                    media_type: media_type.into(),
                    creator,
                    source_revision: None,
                    sensitivity: Sensitivity::Internal,
                    retain_until_ms: self.retain_until_ms,
                },
            )
            .map(|metadata| metadata.resource_ref())
            .map_err(|error| OperationError::Execution(error.to_string()))
    }
}

impl OperationExecutor for ProcessExecutor {
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
            .any(|requirement| requirement.action == CapabilityAction::ProcessExec)
        {
            return Err(OperationError::Schema(
                "process.exec requires a process.exec capability requirement".into(),
            ));
        }
        let input = serde_json::from_value(request.input.clone())
            .map_err(|error| OperationError::Schema(error.to_string()))?;
        self.run(input, &request.actor)
    }
}

struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn drain(
    mut reader: impl Read + Send + 'static,
    limit: u64,
) -> thread::JoinHandle<io::Result<BoundedOutput>> {
    thread::spawn(move || {
        let capacity = usize::try_from(limit).unwrap_or(usize::MAX);
        let mut bytes = Vec::with_capacity(capacity.min(64 * 1024));
        let mut truncated = false;
        let mut chunk = [0; 8192];
        loop {
            let count = reader.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            let remaining = capacity.saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..count.min(remaining)]);
            truncated |= count > remaining;
        }
        Ok(BoundedOutput { bytes, truncated })
    })
}

fn wait_bounded(
    child: &mut Child,
    timeout: Duration,
    grace: Duration,
) -> Result<(ExitStatus, bool, bool, bool, Cleanup), OperationError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().map_err(execution)? {
            return Ok((status, false, false, false, Cleanup::Reaped));
        }
        thread::sleep(Duration::from_millis(5));
    }

    let graceful = terminate(child);
    let grace_deadline = Instant::now() + grace;
    while Instant::now() < grace_deadline {
        if let Some(status) = child.try_wait().map_err(execution)? {
            return Ok((status, true, graceful, false, Cleanup::Terminated));
        }
        thread::sleep(Duration::from_millis(5));
    }
    force_kill(child)?;
    let status = child.wait().map_err(execution)?;
    Ok((status, true, graceful, true, Cleanup::Killed))
}

#[cfg(unix)]
fn terminate(child: &Child) -> bool {
    Command::new("kill")
        .args(["-TERM", &format!("-{}", child.id())])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn force_kill(child: &mut Child) -> Result<(), OperationError> {
    let status = Command::new("kill")
        .args(["-KILL", &format!("-{}", child.id())])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(execution)?;
    if status.success() {
        Ok(())
    } else {
        child.kill().map_err(execution)
    }
}

#[cfg(windows)]
fn terminate(child: &Child) -> bool {
    Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn force_kill(child: &mut Child) -> Result<(), OperationError> {
    let status = Command::new("taskkill")
        .args(["/F", "/PID", &child.id().to_string(), "/T"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(execution)?;
    if status.success() {
        Ok(())
    } else {
        child.kill().map_err(execution)
    }
}

fn execution(error: io::Error) -> OperationError {
    OperationError::Execution(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::artifact::{ArtifactReadLimits, FileArtifactStore};

    fn run(argv: Vec<String>, limit: u64, timeout_ms: u64) -> (ProcessResult, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileArtifactStore::open(dir.path(), 0).unwrap());
        let executor =
            ProcessExecutor::new(store.clone(), Vec::new(), Duration::from_millis(20), 0);
        let outcome = executor
            .run(
                ProcessInput {
                    argv,
                    timeout_ms,
                    max_output_bytes: limit,
                },
                &Principal::System,
            )
            .unwrap();
        let limits = ArtifactReadLimits {
            max_bytes: 4096,
            max_expansion_ratio: 100,
        };
        let result_id = outcome.value.unwrap().value().parse().unwrap();
        let result = serde_json::from_slice(&store.read(result_id, limits).unwrap()).unwrap();
        let stdout_id = outcome.evidence[0].value().parse().unwrap();
        (result, store.read(stdout_id, limits).unwrap())
    }

    #[cfg(unix)]
    #[test]
    fn direct_spawn_bounds_output_and_clears_inherited_environment() {
        let (result, stdout) = run(
            vec![
                "sh".into(),
                "-c".into(),
                "printf %s \"${HOME-unset}\"; printf 123456789".into(),
            ],
            7,
            2_000,
        );

        assert_eq!(stdout, b"unset12");
        assert!(result.stdout_truncated);
        assert_eq!(result.cleanup, Cleanup::Reaped);
        assert!(!result.timed_out);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_records_graceful_termination_and_reaps_the_process() {
        let (result, _) = run(
            vec![
                "sh".into(),
                "-c".into(),
                "trap 'exit 0' TERM; while :; do sleep 1; done".into(),
            ],
            16,
            20,
        );

        assert!(result.timed_out);
        assert!(result.graceful_termination_sent);
        assert!(matches!(
            result.cleanup,
            Cleanup::Terminated | Cleanup::Killed
        ));
        assert_eq!(result.forced_kill_sent, result.cleanup == Cleanup::Killed);
    }
}
