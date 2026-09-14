//! `process.start`, `process.poll`, `process.write`, `process.stop`: a command
//! that outlives the tool call that began it.
//!
//! # Why this is not `process.exec` with a longer timeout
//!
//! [`ProcessExecutor`](crate::process::ProcessExecutor) runs a command to
//! completion and returns everything it printed. That is the right shape for a
//! build or a test suite, and the wrong one for the three things an agent
//! cannot otherwise do at all:
//!
//! - a dev server or a file watcher, which never exits, so waiting for it is
//!   waiting forever;
//! - a long suite whose progress is worth reading before the end;
//! - anything that wants input — a REPL, an interactive installer — which
//!   `process.exec` cannot give, because it closes stdin at spawn.
//!
//! So a start returns a *handle* instead of a result. Output accumulates
//! against that handle, `process.poll` drains what has arrived so far,
//! `process.write` sends a line to the running process, and `process.stop`
//! ends it. The exit code stays readable after the process is gone, which is
//! the whole point of a handle outliving what it names.
//!
//! # Why a pseudoterminal is an option and not the default
//!
//! A pipe is cheaper and its output is exactly what the program wrote. Some
//! programs ask `isatty` and behave differently when the answer is no — that
//! is usually helpful (no colour codes, no progress spinner) and occasionally
//! fatal (no prompt, so a REPL reads nothing). `pty: true` is for the second
//! case, and it costs a real terminal's worth of control characters in the
//! output, so it is not what a caller gets without asking.
//!
//! # Lifetime
//!
//! A session is a process on this machine, so its registry is a process-wide
//! map rather than anything durable: a handle cannot outlive the harness that
//! holds the child. Each session records the scope that created it and refuses
//! every other, so one task cannot poll or kill another's process by guessing
//! a handle.

use arsy_kernel::{
    artifact::{ArtifactStore, NewArtifact, Sensitivity},
    capability::{CapabilityAction, CapabilityGrant},
    domain::{Principal, ResourceRef},
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

/// How much unread output one session retains. Poll drains, so this is only
/// reached by a caller that starts something noisy and never reads it.
const DEFAULT_BUFFER_BYTES: u64 = 1024 * 1024;
const MAX_BUFFER_BYTES: u64 = 16 * 1024 * 1024;

/// A background command may live far longer than a foreground one, but not
/// forever: an abandoned dev server is still holding a port.
const DEFAULT_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;

/// How often the watchdog looks at its child. Short enough that `stop` feels
/// immediate, long enough that an idle session costs nothing measurable.
const TICK: Duration = Duration::from_millis(25);

/// What a poll reports.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PollResult {
    pub handle: String,
    pub argv: Vec<String>,
    pub pid: u32,
    pub running: bool,
    /// `None` while it runs, and for a process a signal ended.
    pub status_code: Option<i32>,
    pub timed_out: bool,
    /// Whether `process.stop` asked for this exit.
    pub stopped: bool,
    /// Output since the previous poll, in the order it was printed.
    pub output: String,
    /// Bytes this session has produced in total, read or not.
    pub total_output_bytes: u64,
    /// Whether output was dropped because nothing polled in time.
    pub dropped_output: bool,
    pub pty: bool,
}

/// What a start reports. Deliberately not the first bytes of output: a caller
/// that wants those polls for them, and a start that sometimes returned output
/// and sometimes did not would be a race dressed up as a convenience.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StartResult {
    pub handle: String,
    pub argv: Vec<String>,
    pub pid: u32,
    pub pty: bool,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WriteResult {
    pub handle: String,
    pub bytes_written: usize,
}

#[derive(Default)]
struct Buffer {
    pending: Vec<u8>,
    total: u64,
    dropped: bool,
}

/// How a finished process ended, kept after the child is gone.
#[derive(Clone, Copy, Debug, Default)]
struct Exit {
    status_code: Option<i32>,
    timed_out: bool,
    stopped: bool,
}

struct Session {
    handle: String,
    argv: Vec<String>,
    /// Which registry started it. A handle is only usable from there.
    scope: String,
    workspace: PathBuf,
    pty: bool,
    pid: u32,
    buffer: Arc<Mutex<Buffer>>,
    /// The child's end of the conversation: the pipe, or the pseudoterminal.
    input: Mutex<Option<Box<dyn Write + Send>>>,
    stop_requested: Arc<AtomicBool>,
    exit: Arc<Mutex<Option<Exit>>>,
}

impl Session {
    fn exited(&self) -> Option<Exit> {
        *self.exit.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// Every live and finished session in this process.
///
/// Keyed by handle rather than by scope so [`program_for`] can answer the
/// capability question — "what program does this handle name?" — without
/// knowing which registry is asking.
static SESSIONS: OnceLock<Mutex<HashMap<String, Arc<Session>>>> = OnceLock::new();

fn sessions() -> std::sync::MutexGuard<'static, HashMap<String, Arc<Session>>> {
    SESSIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// The program a handle names, for the capability requirement of a call that
/// only carries the handle.
///
/// Without this, polling `cargo test` would ask for authority over `process:*`
/// while starting it asked for authority over `process:cargo`, and an operator
/// who narrowed `process.exec` to the commands they trust would find that the
/// narrow rule let them start a process they could not then read.
pub fn program_for(handle: &str) -> Option<String> {
    sessions()
        .get(handle)
        .and_then(|session| session.argv.first().cloned())
}

/// What a background-process call may ask for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionOperation {
    Start,
    Poll,
    Write,
    Stop,
}

impl SessionOperation {
    pub const ALL: [Self; 4] = [Self::Start, Self::Poll, Self::Write, Self::Stop];

    const fn kind(self) -> &'static str {
        match self {
            Self::Start => "process.start",
            Self::Poll => "process.poll",
            Self::Write => "process.write",
            Self::Stop => "process.stop",
        }
    }

    /// Stopping is signalling; the rest is driving a process this actor is
    /// allowed to run. Both are answered against the program, not the handle,
    /// so one rule covers a command's whole life.
    const fn action(self) -> CapabilityAction {
        match self {
            Self::Stop => CapabilityAction::ProcessSignal,
            Self::Start | Self::Poll | Self::Write => CapabilityAction::ProcessExec,
        }
    }

    /// Nothing here is idempotent: a poll consumes the output it reports, and
    /// replaying a start would leave a second process running.
    const fn reversible(self) -> bool {
        match self {
            // Reading what has already been printed changes no state outside
            // the buffer it drains, and a stop can be re-issued harmlessly.
            Self::Poll | Self::Stop => true,
            Self::Start | Self::Write => false,
        }
    }

    fn schema(self) -> InputSchema {
        let string = |name: &str| (name.to_owned(), JsonType::String);
        let (required, optional) = match self {
            Self::Start => (
                vec![("argv".to_owned(), JsonType::Array)],
                vec![
                    ("pty".to_owned(), JsonType::Boolean),
                    ("timeout_ms".to_owned(), JsonType::Number),
                    ("max_output_bytes".to_owned(), JsonType::Number),
                ],
            ),
            Self::Poll | Self::Stop => (vec![string("handle")], Vec::new()),
            Self::Write => (vec![string("handle"), string("data")], Vec::new()),
        };
        InputSchema {
            required: required.into_iter().collect(),
            optional: optional.into_iter().collect(),
            allow_extra: false,
        }
    }
}

pub struct SessionExecutor {
    operation: SessionOperation,
    contract: OperationContract,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
    workspace: PathBuf,
    scope: String,
    environment: Vec<(String, String)>,
    grace: Duration,
}

impl SessionExecutor {
    /// One executor per kind, all sharing the process-wide session registry.
    pub fn executors(
        workspace: &crate::resource::Workspace,
        artifacts: &Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
        scope: &str,
        environment: Vec<(String, String)>,
        grace: Duration,
    ) -> Vec<Arc<dyn OperationExecutor>> {
        SessionOperation::ALL
            .into_iter()
            .map(|operation| {
                Arc::new(Self {
                    operation,
                    contract: OperationContract {
                        kind: OperationKind::new(operation.kind())
                            .expect("static operation kind is valid"),
                        input_schema: operation.schema(),
                        actions: vec![operation.action()],
                        idempotency: Idempotency::Effectful,
                        reversible: operation.reversible(),
                        concurrency: ConcurrencyRule::Parallel,
                    },
                    artifacts: Arc::clone(artifacts),
                    retain_until_ms,
                    workspace: workspace.path().to_path_buf(),
                    scope: scope.to_owned(),
                    environment: environment.clone(),
                    grace,
                }) as Arc<dyn OperationExecutor>
            })
            .collect()
    }

    fn put(&self, value: &impl Serialize) -> Result<ResourceRef, OperationError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| OperationError::Execution(error.to_string()))?;
        self.artifacts
            .put(
                &bytes,
                NewArtifact {
                    media_type: "application/json".into(),
                    creator: Principal::System,
                    source_revision: None,
                    sensitivity: Sensitivity::Internal,
                    retain_until_ms: self.retain_until_ms,
                },
            )
            .map(|metadata| metadata.resource_ref())
            .map_err(|error| OperationError::Execution(error.to_string()))
    }

    /// The session a handle names, refusing one another scope started.
    fn session(&self, input: &Value) -> Result<Arc<Session>, OperationError> {
        let handle = input
            .get("handle")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let session = sessions().get(handle).cloned().ok_or_else(|| {
            OperationError::Execution(format!(
                "no background process is registered as `{handle}`; start one with process.start"
            ))
        })?;
        if session.scope != self.scope || session.workspace != self.workspace {
            return Err(OperationError::Execution(format!(
                "`{handle}` belongs to another task's session"
            )));
        }
        Ok(session)
    }

    fn start(&self, input: &Value) -> Result<StartResult, OperationError> {
        let argv: Vec<String> = input
            .get("argv")
            .and_then(Value::as_array)
            .map(|argv| {
                argv.iter()
                    .map(|value| value.as_str().unwrap_or_default().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let (program, args) = argv
            .split_first()
            .filter(|(program, _)| !program.is_empty())
            .ok_or_else(|| OperationError::Schema("argv must name a program".into()))?;
        let pty = input.get("pty").and_then(Value::as_bool).unwrap_or(false);
        if pty && !arsy_pty::supported() {
            return Err(OperationError::Execution(
                "this build cannot allocate a pseudoterminal; start the process without `pty`"
                    .into(),
            ));
        }
        let timeout = Duration::from_millis(
            input
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1, DEFAULT_TIMEOUT_MS),
        );
        let capacity = input
            .get("max_output_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_BUFFER_BYTES)
            .clamp(1, MAX_BUFFER_BYTES);

        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(&self.workspace)
            .env_clear()
            .envs(self.environment.iter().map(|(k, v)| (k, v)));

        let buffer = Arc::new(Mutex::new(Buffer::default()));
        let (mut child, input_sink, readers) = if pty {
            self.spawn_on_pty(&mut command, &buffer, capacity)?
        } else {
            spawn_on_pipes(&mut command, &buffer, capacity)?
        };
        let pid = child.id();
        let handle = format!("proc-{}", arsy_kernel::domain::OperationId::new());

        let stop_requested = Arc::new(AtomicBool::new(false));
        let exit = Arc::new(Mutex::new(None));
        {
            let stop_requested = Arc::clone(&stop_requested);
            let exit = Arc::clone(&exit);
            let grace = self.grace;
            // The watchdog is the only owner of the `Child`, so nothing else
            // needs a lock to wait on it, and the exit status is recorded
            // exactly once whether the process finished, was stopped, or ran
            // past its deadline.
            thread::spawn(move || {
                let recorded = supervise(&mut child, timeout, grace, &stop_requested);
                for reader in readers {
                    let _ = reader.join();
                }
                *exit.lock().unwrap_or_else(|error| error.into_inner()) = Some(recorded);
            });
        }

        let session = Arc::new(Session {
            handle: handle.clone(),
            argv: argv.clone(),
            scope: self.scope.clone(),
            workspace: self.workspace.clone(),
            pty,
            pid,
            buffer,
            input: Mutex::new(input_sink),
            stop_requested,
            exit,
        });
        sessions().insert(handle.clone(), session);
        Ok(StartResult {
            handle,
            argv,
            pid,
            pty,
        })
    }

    #[cfg(unix)]
    fn spawn_on_pty(
        &self,
        command: &mut Command,
        buffer: &Arc<Mutex<Buffer>>,
        capacity: u64,
    ) -> Result<Spawned, OperationError> {
        let pty = arsy_pty::Pty::open().map_err(execution)?;
        pty.attach(command).map_err(execution)?;
        let child = command.spawn().map_err(execution)?;
        // Dropped now the child holds its own copy: while the harness still
        // has the device end open the terminal has a writer that never writes,
        // and a read would block past the child's exit.
        let controller = pty.close_device();
        let sink = controller.try_clone().map_err(execution)?;
        let reader = drain(controller, Arc::clone(buffer), capacity);
        Ok((child, Some(Box::new(sink)), vec![reader]))
    }

    #[cfg(not(unix))]
    fn spawn_on_pty(
        &self,
        _command: &mut Command,
        _buffer: &Arc<Mutex<Buffer>>,
        _capacity: u64,
    ) -> Result<Spawned, OperationError> {
        Err(OperationError::Execution(
            "this build cannot allocate a pseudoterminal".into(),
        ))
    }

    fn poll(&self, input: &Value) -> Result<PollResult, OperationError> {
        let session = self.session(input)?;
        let (output, total, dropped) = {
            let mut buffer = session
                .buffer
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let taken = std::mem::take(&mut buffer.pending);
            let dropped = std::mem::take(&mut buffer.dropped);
            (
                String::from_utf8_lossy(&taken).into_owned(),
                buffer.total,
                dropped,
            )
        };
        let exit = session.exited();
        // A finished session is forgotten once its last output has been read,
        // so a long run does not accumulate dead handles — but only then, so
        // the exit code of something that just failed is still readable.
        if exit.is_some() && output.is_empty() {
            sessions().remove(&session.handle);
        }
        Ok(PollResult {
            handle: session.handle.clone(),
            argv: session.argv.clone(),
            pid: session.pid,
            running: exit.is_none(),
            status_code: exit.and_then(|exit| exit.status_code),
            timed_out: exit.is_some_and(|exit| exit.timed_out),
            stopped: exit.is_some_and(|exit| exit.stopped),
            output,
            total_output_bytes: total,
            dropped_output: dropped,
            pty: session.pty,
        })
    }

    fn write(&self, input: &Value) -> Result<WriteResult, OperationError> {
        let session = self.session(input)?;
        if session.exited().is_some() {
            return Err(OperationError::Execution(format!(
                "`{}` has already exited; nothing is reading its input",
                session.handle
            )));
        }
        let data = input
            .get("data")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut guard = session
            .input
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let sink = guard.as_mut().ok_or_else(|| {
            OperationError::Execution(format!(
                "`{}` was started without an input stream",
                session.handle
            ))
        })?;
        sink.write_all(data.as_bytes()).map_err(execution)?;
        sink.flush().map_err(execution)?;
        Ok(WriteResult {
            handle: session.handle.clone(),
            bytes_written: data.len(),
        })
    }

    /// Ask the watchdog to end the process, and wait for it to say it did.
    ///
    /// Waiting rather than returning immediately is what makes the exit code
    /// part of the answer: a caller that stops a server and is told nothing
    /// about how it ended has to poll to find out, and the poll may arrive
    /// before the watchdog has reaped anything.
    fn stop(&self, input: &Value) -> Result<PollResult, OperationError> {
        let session = self.session(input)?;
        session.stop_requested.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + self.grace + TICK * 20;
        while session.exited().is_none() && Instant::now() < deadline {
            thread::sleep(TICK);
        }
        self.poll(input)
    }
}

type Spawned = (
    Child,
    Option<Box<dyn Write + Send>>,
    Vec<thread::JoinHandle<()>>,
);

fn spawn_on_pipes(
    command: &mut Command,
    buffer: &Arc<Mutex<Buffer>>,
    capacity: u64,
) -> Result<Spawned, OperationError> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own group, so ending the session ends what the command started
        // as well as the command itself. A pseudoterminal session does not do
        // this here: `setsid` gives it a new session — which is a new group
        // too — and calling both makes the second fail with EPERM.
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(execution)?;
    let stdout = child.stdout.take().expect("piped stdout is present");
    let stderr = child.stderr.take().expect("piped stderr is present");
    let stdin = child.stdin.take().expect("piped stdin is present");
    let readers = vec![
        drain(stdout, Arc::clone(buffer), capacity),
        drain(stderr, Arc::clone(buffer), capacity),
    ];
    Ok((child, Some(Box::new(stdin)), readers))
}

/// Accumulate one stream into the session's buffer until it closes.
///
/// Both streams share the buffer, so what the caller reads is interleaved the
/// way the terminal would have shown it rather than split into two reports
/// that have to be re-ordered by guesswork.
fn drain(
    mut reader: impl Read + Send + 'static,
    buffer: Arc<Mutex<Buffer>>,
    capacity: u64,
) -> thread::JoinHandle<()> {
    let capacity = usize::try_from(capacity).unwrap_or(usize::MAX);
    thread::spawn(move || {
        let mut chunk = [0; 8192];
        loop {
            // A closed pseudoterminal reports EIO rather than end-of-file on
            // some platforms, so any error ends the stream the same way a
            // clean close does.
            let Ok(count) = reader.read(&mut chunk) else {
                break;
            };
            if count == 0 {
                break;
            }
            let mut buffer = buffer.lock().unwrap_or_else(|error| error.into_inner());
            buffer.total = buffer.total.saturating_add(count as u64);
            let room = capacity.saturating_sub(buffer.pending.len());
            buffer.pending.extend_from_slice(&chunk[..count.min(room)]);
            // Oldest kept, newest dropped: the same choice `process.exec`
            // makes, and the flag says it happened so a caller polling too
            // slowly is not left to infer it from a gap.
            buffer.dropped |= count > room;
        }
    })
}

/// Own the child until it ends, whichever way it ends.
fn supervise(
    child: &mut Child,
    timeout: Duration,
    grace: Duration,
    stop_requested: &AtomicBool,
) -> Exit {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Exit {
                    status_code: status.code(),
                    ..Exit::default()
                }
            }
            Err(_) => return Exit::default(),
            Ok(None) => {}
        }
        let stopped = stop_requested.load(Ordering::SeqCst);
        let timed_out = Instant::now() >= deadline;
        if stopped || timed_out {
            let ended = crate::process::end(child, grace);
            return Exit {
                status_code: ended.status.and_then(|status| status.code()),
                timed_out,
                stopped,
            };
        }
        thread::sleep(TICK);
    }
}

fn execution(error: std::io::Error) -> OperationError {
    OperationError::Execution(error.to_string())
}

impl OperationExecutor for SessionExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let input = &request.input;
        let (value, program) = match self.operation {
            SessionOperation::Start => {
                let result = self.start(input)?;
                let program = result.argv.first().cloned().unwrap_or_default();
                (self.put(&result)?, program)
            }
            SessionOperation::Poll => {
                let result = self.poll(input)?;
                let program = result.argv.first().cloned().unwrap_or_default();
                (self.put(&result)?, program)
            }
            SessionOperation::Write => {
                let result = self.write(input)?;
                let program = program_for(&result.handle).unwrap_or_default();
                (self.put(&result)?, program)
            }
            SessionOperation::Stop => {
                let result = self.stop(input)?;
                let program = result.argv.first().cloned().unwrap_or_default();
                (self.put(&result)?, program)
            }
        };
        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: self.operation.action(),
                resource: ResourceRef::new("process", program)
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
            }],
            evidence: Vec::new(),
            state: None,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use arsy_kernel::{artifact::FileArtifactStore, domain::OperationId};

    struct Harness {
        _directory: tempfile::TempDir,
        executors: Vec<Arc<dyn OperationExecutor>>,
        artifacts: Arc<dyn ArtifactStore>,
    }

    fn harness(scope: &str) -> Harness {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::resource::Workspace::open(directory.path()).unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(directory.path().join("artifacts"), 0).unwrap());
        let executors = SessionExecutor::executors(
            &workspace,
            &artifacts,
            0,
            scope,
            vec![("PATH".to_owned(), std::env::var("PATH").unwrap_or_default())],
            Duration::from_millis(200),
        );
        Harness {
            _directory: directory,
            executors,
            artifacts,
        }
    }

    impl Harness {
        fn call(&self, kind: &str, input: Value) -> Result<Value, OperationError> {
            let executor = self
                .executors
                .iter()
                .find(|executor| executor.contract().kind.as_str() == kind)
                .unwrap_or_else(|| panic!("no executor for {kind}"));
            let request = OperationRequest {
                id: OperationId::new(),
                kind: executor.contract().kind.clone(),
                actor: Principal::System,
                requirements: Vec::new(),
                input,
            };
            let outcome = executor.execute(&request, &[])?;
            let id: arsy_kernel::domain::ArtifactId = outcome
                .value
                .expect("every session call returns a result")
                .value()
                .parse()
                .unwrap();
            let bytes = self
                .artifacts
                .read(
                    id,
                    arsy_kernel::artifact::ArtifactReadLimits {
                        max_bytes: 4 * 1024 * 1024,
                        max_expansion_ratio: 1_000,
                    },
                )
                .unwrap();
            Ok(serde_json::from_slice(&bytes).unwrap())
        }

        /// Poll until the session reports it is no longer running, or give up.
        fn poll_until_done(&self, handle: &str) -> Value {
            let mut collected = String::new();
            for _ in 0..400 {
                let result = self
                    .call("process.poll", serde_json::json!({"handle": handle}))
                    .unwrap();
                collected.push_str(result["output"].as_str().unwrap_or_default());
                if result["running"] == Value::Bool(false) {
                    let mut result = result;
                    result["output"] = Value::String(collected);
                    return result;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("`{handle}` never finished");
        }
    }

    #[test]
    fn a_started_command_keeps_its_output_and_exit_code_against_one_handle() {
        let harness = harness("start-poll");
        let started = harness
            .call(
                "process.start",
                serde_json::json!({"argv": ["sh", "-c", "printf hello; exit 3"]}),
            )
            .unwrap();
        let handle = started["handle"].as_str().unwrap().to_owned();
        assert!(started["pid"].as_u64().unwrap() > 0);

        let done = harness.poll_until_done(&handle);
        assert_eq!(done["output"], "hello");
        assert_eq!(done["status_code"], 3);
        assert_eq!(done["timed_out"], false);
        assert_eq!(done["handle"], handle.as_str());
    }

    #[test]
    fn a_process_reads_what_is_written_to_it_after_it_started() {
        let harness = harness("write");
        let started = harness
            .call(
                "process.start",
                serde_json::json!({"argv": ["sh", "-c", "read line; printf 'got %s' \"$line\""]}),
            )
            .unwrap();
        let handle = started["handle"].as_str().unwrap().to_owned();

        let written = harness
            .call(
                "process.write",
                serde_json::json!({"handle": handle, "data": "ping\n"}),
            )
            .unwrap();
        assert_eq!(written["bytes_written"], 5);

        let done = harness.poll_until_done(&handle);
        assert_eq!(done["output"], "got ping");
        assert_eq!(done["status_code"], 0);
    }

    /// The point of the option: a program that asks whether it is talking to a
    /// terminal gets a different answer than it would down a pipe.
    #[test]
    fn a_pty_session_is_a_terminal_and_a_pipe_session_is_not() {
        let harness = harness("pty");
        let ask = serde_json::json!({
            "argv": ["sh", "-c", "if [ -t 0 ]; then printf tty; else printf pipe; fi"],
        });

        let piped = harness.call("process.start", ask.clone()).unwrap();
        let piped = harness.poll_until_done(piped["handle"].as_str().unwrap());
        assert_eq!(piped["output"], "pipe");
        assert_eq!(piped["pty"], false);

        let mut with_pty = ask;
        with_pty["pty"] = Value::Bool(true);
        let started = harness.call("process.start", with_pty).unwrap();
        assert_eq!(started["pty"], true);
        let done = harness.poll_until_done(started["handle"].as_str().unwrap());
        // A terminal ends a line with CRLF, so the text is matched rather than
        // compared: the difference being tested is `tty` against `pipe`.
        assert!(
            done["output"].as_str().unwrap().contains("tty"),
            "a pseudoterminal session reported {}",
            done["output"]
        );
    }

    #[test]
    fn stopping_by_handle_ends_the_process_and_reports_how_it_ended() {
        let harness = harness("stop");
        let started = harness
            .call(
                "process.start",
                serde_json::json!({"argv": ["sh", "-c", "while :; do sleep 1; done"]}),
            )
            .unwrap();
        let handle = started["handle"].as_str().unwrap().to_owned();

        let stopped = harness
            .call("process.stop", serde_json::json!({"handle": handle}))
            .unwrap();
        assert_eq!(stopped["running"], false);
        assert_eq!(stopped["stopped"], true);

        // Writing to something that has exited is refused rather than silently
        // discarded, so a caller is told its input went nowhere.
        assert!(harness
            .call(
                "process.write",
                serde_json::json!({"handle": handle, "data": "x\n"})
            )
            .is_err());
    }

    #[test]
    fn a_deadline_ends_a_command_that_would_otherwise_run_forever() {
        let harness = harness("timeout");
        let started = harness
            .call(
                "process.start",
                serde_json::json!({
                    "argv": ["sh", "-c", "while :; do sleep 1; done"],
                    "timeout_ms": 50,
                }),
            )
            .unwrap();
        let done = harness.poll_until_done(started["handle"].as_str().unwrap());
        assert_eq!(done["timed_out"], true);
        assert_eq!(done["stopped"], false);
    }

    /// A handle is not a password: one task must not be able to reach another's
    /// process by holding its id.
    #[test]
    fn a_handle_is_useless_outside_the_scope_that_created_it() {
        let mine = harness("owner");
        let started = mine
            .call(
                "process.start",
                serde_json::json!({"argv": ["sh", "-c", "sleep 5"]}),
            )
            .unwrap();
        let handle = started["handle"].as_str().unwrap().to_owned();

        let theirs = harness("intruder");
        let refused = theirs
            .call(
                "process.poll",
                serde_json::json!({"handle": handle.clone()}),
            )
            .unwrap_err();
        assert!(
            refused.to_string().contains("another task"),
            "a foreign handle was not refused: {refused}"
        );

        // The program behind a handle is what a capability question is asked
        // about, whichever scope is asking.
        assert_eq!(program_for(&handle).as_deref(), Some("sh"));
        let _ = mine.call("process.stop", serde_json::json!({"handle": handle}));
    }
}
