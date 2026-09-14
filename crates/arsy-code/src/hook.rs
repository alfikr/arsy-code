//! The hook lifecycle engine.
//!
//! See `docs/19-plugin-extension-system.md`. A hook observes a lifecycle event
//! and may transform its payload, deny it, ask for approval, inject attributed
//! context, or schedule one follow-up. Three properties keep that from becoming
//! a way around the rest of the system:
//!
//! * **A hook cannot grant.** Outcomes are ordered by how much they restrict.
//!   An `Allow` from an origin that may not grant authority is downgraded to
//!   `Continue` and reported, exactly as `policy::RuleSet::compile` downgrades
//!   a rule. Denying is always permitted: restriction needs no authority.
//! * **A hook cannot loop.** Dispatch is bounded by depth, and a reentrancy key
//!   refuses an event that is already on the stack — which is what a hook that
//!   re-triggers its own event would produce.
//! * **A hook cannot hang.** Every handler runs under the rule's deadline, and
//!   what happens when one fails is decided per event, up front.

use arsy_kernel::capability::PolicySource;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fmt,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

/// Lifecycle events a hook may observe. Closed, because a declaration naming
/// an event this build does not have must fail to register rather than sit
/// silently unreachable.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEvent {
    SessionStarted,
    SessionEnded,
    BeforeTurn,
    AfterTurn,
    BeforeOperation,
    AfterOperation,
    OperationFailed,
    BeforeCompaction,
}

impl LifecycleEvent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStarted => "session_started",
            Self::SessionEnded => "session_ended",
            Self::BeforeTurn => "before_turn",
            Self::AfterTurn => "after_turn",
            Self::BeforeOperation => "before_operation",
            Self::AfterOperation => "after_operation",
            Self::OperationFailed => "operation_failed",
            Self::BeforeCompaction => "before_compaction",
        }
    }

    /// The canonical name a compatibility import maps onto, or `None` for one
    /// this build does not implement.
    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::SessionStarted,
            Self::SessionEnded,
            Self::BeforeTurn,
            Self::AfterTurn,
            Self::BeforeOperation,
            Self::AfterOperation,
            Self::OperationFailed,
            Self::BeforeCompaction,
        ]
        .into_iter()
        .find(|event| event.as_str() == value)
    }

    /// The event a Claude-shaped declaration names, or `None` for one this
    /// build does not implement.
    ///
    /// The single table for that mapping. `compat` reports declarations and
    /// this module runs them; two tables would eventually disagree, and the
    /// disagreement would be a hook that `arsy hook list` shows and the engine
    /// never dispatches.
    pub fn from_external(value: &str) -> Option<Self> {
        Some(match value {
            "PreToolUse" => Self::BeforeOperation,
            "PostToolUse" => Self::AfterOperation,
            "PostToolUseFailure" => Self::OperationFailed,
            "SessionStart" => Self::SessionStarted,
            "SessionEnd" => Self::SessionEnded,
            "UserPromptSubmit" => Self::BeforeTurn,
            "Stop" => Self::AfterTurn,
            "PreCompact" => Self::BeforeCompaction,
            _ => return None,
        })
    }

    /// What happens when a hook on this event fails or times out.
    ///
    /// An event that gates something — a turn, an operation, a compaction —
    /// fails closed: a hook that was meant to be able to deny must not be
    /// bypassable by crashing. An event that only reports what already happened
    /// fails open, because refusing it would undo nothing.
    pub const fn failure_policy(self) -> FailurePolicy {
        match self {
            Self::BeforeTurn | Self::BeforeOperation | Self::BeforeCompaction => {
                FailurePolicy::FailClosed
            }
            Self::SessionStarted
            | Self::SessionEnded
            | Self::AfterTurn
            | Self::AfterOperation
            | Self::OperationFailed => FailurePolicy::FailOpen,
        }
    }
}

impl fmt::Display for LifecycleEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    /// A failed hook denies the event.
    FailClosed,
    /// A failed hook is reported and the event continues.
    FailOpen,
}

/// What a rule declares it may do. Published so `arsy hook list` can report the
/// effect class before anything runs.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    /// Reads the payload and returns nothing.
    Observe,
    /// May rewrite the payload it was given.
    Transform,
    /// May stop the event, or ask for approval.
    Gate,
}

/// One registered hook.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HookRule {
    pub id: String,
    pub event: LifecycleEvent,
    /// Glob over the event's subject — an operation kind, a command name. `*`
    /// matches every subject.
    pub matcher: String,
    pub effect: EffectClass,
    /// The authority of whatever declared this rule.
    pub origin: PolicySource,
    pub timeout: Duration,
}

impl HookRule {
    fn matches(&self, subject: &str) -> bool {
        glob(&self.matcher, subject)
    }
}

/// Leading/trailing `*` matching, which is the whole matcher vocabulary the
/// imported ecosystems use. A full glob engine here would be a second pattern
/// language beside `capability::ResourcePattern`, for no case that needs one.
fn glob(pattern: &str, subject: &str) -> bool {
    match (pattern.strip_prefix('*'), pattern.strip_suffix('*')) {
        (Some("") | None, Some("")) | (Some(""), None) => true,
        (Some(rest), Some(_)) => subject.contains(rest.trim_end_matches('*')),
        (Some(rest), None) => subject.ends_with(rest),
        (None, Some(rest)) => subject.starts_with(rest),
        (None, None) => pattern == subject,
    }
}

/// What one hook decided. Ordered by how much it restricts, so merging several
/// is `min`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// Stop the event.
    Deny(String),
    /// Let the operator decide.
    RequireApproval(String),
    /// Nothing to say.
    Continue,
    /// Permit something that would otherwise be gated. Needs an origin that
    /// may grant authority.
    Allow,
}

impl Outcome {
    const fn rank(&self) -> u8 {
        match self {
            Self::Deny(_) => 0,
            Self::RequireApproval(_) => 1,
            Self::Continue => 2,
            Self::Allow => 3,
        }
    }
}

/// Everything a handler may return alongside its verdict.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HandlerResult {
    pub outcome: Option<Outcome>,
    /// A rewritten payload. Ignored for a rule that did not declare
    /// `Transform` or `Gate`.
    pub payload: Option<Value>,
    /// Context to add to the turn, attributed to the rule that injected it.
    pub inject: Option<String>,
    /// At most one follow-up per dispatch.
    pub schedule: Option<Value>,
}

/// How a rule's handler is actually run. Injected so the engine's guards are
/// exercisable without a subprocess.
pub trait HookHandler: Send + Sync {
    fn run(&self, rule: &HookRule, payload: &Value) -> Result<HandlerResult, HookError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookError {
    /// Dispatch nested deeper than the engine allows.
    RecursionLimit(u32),
    /// The event is already on the stack for this subject.
    Reentrant { event: LifecycleEvent, key: String },
    /// The handler exceeded the rule's deadline.
    Timeout { rule: String, limit: Duration },
    /// The handler failed for its own reasons.
    Handler { rule: String, message: String },
    /// A second follow-up in one dispatch.
    TooManyFollowUps(String),
}

impl fmt::Display for HookError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RecursionLimit(depth) => {
                write!(formatter, "hook dispatch nested deeper than {depth}")
            }
            Self::Reentrant { event, key } => {
                write!(formatter, "`{event}` is already dispatching for `{key}`")
            }
            Self::Timeout { rule, limit } => {
                write!(formatter, "hook `{rule}` exceeded {limit:?}")
            }
            Self::Handler { rule, message } => write!(formatter, "hook `{rule}` failed: {message}"),
            Self::TooManyFollowUps(rule) => write!(
                formatter,
                "hook `{rule}` scheduled a second follow-up; one is the limit"
            ),
        }
    }
}

impl std::error::Error for HookError {}

/// What a dispatch decided, and everything it accumulated on the way.
#[derive(Clone, Debug, PartialEq)]
pub struct Dispatch {
    pub outcome: Outcome,
    pub payload: Value,
    /// Injected context, each attributed to the rule that produced it.
    pub injected: Vec<(String, String)>,
    pub follow_up: Option<(String, Value)>,
    /// Rules that ran, in order.
    pub ran: Vec<String>,
    /// Anything refused or downgraded, with the reason.
    pub diagnostics: Vec<String>,
}

/// The registry and the dispatcher.
pub struct HookEngine {
    rules: Vec<(HookRule, Box<dyn HookHandler>)>,
    max_depth: u32,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    depth: u32,
    active: BTreeSet<String>,
}

impl HookEngine {
    /// `max_depth` bounds how far a hook may cause another dispatch. One means
    /// hooks run, but nothing they do can dispatch again.
    pub fn new(max_depth: u32) -> Self {
        Self {
            rules: Vec::new(),
            max_depth,
            state: Mutex::new(State::default()),
        }
    }

    /// Register a rule. Rules run in registration order within one authority,
    /// most authoritative first, so an operator's hook sees the payload before
    /// a repository's does.
    pub fn register(&mut self, rule: HookRule, handler: Box<dyn HookHandler>) {
        let position = self
            .rules
            .iter()
            .position(|(existing, _)| existing.origin > rule.origin)
            .unwrap_or(self.rules.len());
        self.rules.insert(position, (rule, handler));
    }

    pub fn rules(&self) -> impl Iterator<Item = &HookRule> {
        self.rules.iter().map(|(rule, _)| rule)
    }

    /// Run every rule registered for `event` whose matcher covers `subject`.
    ///
    /// The payload threads through the chain: a rule that transforms it hands
    /// the rewritten value to the next one, so the last rule sees what would
    /// actually be used. A denial stops the chain — nothing after it needs to
    /// observe an event that is not going to happen.
    pub fn dispatch(
        &self,
        event: LifecycleEvent,
        subject: &str,
        payload: Value,
    ) -> Result<Dispatch, HookError> {
        let key = format!("{event}:{subject}");
        self.enter(event, &key)?;
        let result = self.run_chain(event, subject, payload);
        self.leave(&key);
        result
    }

    fn run_chain(
        &self,
        event: LifecycleEvent,
        subject: &str,
        payload: Value,
    ) -> Result<Dispatch, HookError> {
        let mut dispatch = Dispatch {
            outcome: Outcome::Continue,
            payload,
            injected: Vec::new(),
            follow_up: None,
            ran: Vec::new(),
            diagnostics: Vec::new(),
        };
        for (rule, handler) in &self.rules {
            if rule.event != event || !rule.matches(subject) {
                continue;
            }
            dispatch.ran.push(rule.id.clone());
            let result = match handler.run(rule, &dispatch.payload) {
                Ok(result) => result,
                Err(error) => {
                    dispatch.diagnostics.push(error.to_string());
                    match event.failure_policy() {
                        // A gate that cannot run has not approved anything.
                        FailurePolicy::FailClosed => {
                            dispatch.outcome = Outcome::Deny(error.to_string());
                            return Ok(dispatch);
                        }
                        FailurePolicy::FailOpen => continue,
                    }
                }
            };
            self.apply(rule, result, &mut dispatch)?;
            if matches!(dispatch.outcome, Outcome::Deny(_)) {
                return Ok(dispatch);
            }
        }
        Ok(dispatch)
    }

    /// Fold one handler's result into the dispatch, enforcing what the rule was
    /// allowed to do.
    fn apply(
        &self,
        rule: &HookRule,
        result: HandlerResult,
        dispatch: &mut Dispatch,
    ) -> Result<(), HookError> {
        if let Some(outcome) = result.outcome {
            // Restriction needs no authority; permission does. A hook whose
            // origin cannot grant is not a way to acquire what policy refused.
            let outcome = match outcome {
                Outcome::Allow if !rule.origin.may_grant() => {
                    dispatch.diagnostics.push(format!(
                        "hook `{}`: allow ignored, a {} hook cannot grant authority",
                        rule.id, rule.origin
                    ));
                    Outcome::Continue
                }
                other => other,
            };
            match outcome {
                // A grant is the one outcome that loosens, so ranking cannot
                // merge it: `min` over the ranks left `Allow` unable to beat
                // the `Continue` a dispatch starts at, which made both the
                // variant and the guard above it dead. It lifts a dispatch
                // that nothing has objected to, and never overrules a denial
                // or an approval another hook asked for.
                Outcome::Allow => {
                    if dispatch.outcome == Outcome::Continue {
                        dispatch.outcome = Outcome::Allow;
                    }
                }
                other if other.rank() < dispatch.outcome.rank() => dispatch.outcome = other,
                _ => {}
            }
        }
        if let Some(payload) = result.payload {
            if rule.effect == EffectClass::Observe {
                dispatch.diagnostics.push(format!(
                    "hook `{}`: payload rewrite ignored, it declared `observe`",
                    rule.id
                ));
            } else {
                dispatch.payload = payload;
            }
        }
        if let Some(text) = result.inject {
            dispatch.injected.push((rule.id.clone(), text));
        }
        if let Some(follow_up) = result.schedule {
            if dispatch.follow_up.is_some() {
                return Err(HookError::TooManyFollowUps(rule.id.clone()));
            }
            dispatch.follow_up = Some((rule.id.clone(), follow_up));
        }
        Ok(())
    }

    fn enter(&self, event: LifecycleEvent, key: &str) -> Result<(), HookError> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| {
            // A panicking handler must not disable every later hook: the guard
            // state is two counters, and both are rebuilt by this dispatch.
            poisoned.into_inner()
        });
        if state.depth >= self.max_depth {
            return Err(HookError::RecursionLimit(self.max_depth));
        }
        if !state.active.insert(key.to_owned()) {
            return Err(HookError::Reentrant {
                event,
                key: key.to_owned(),
            });
        }
        state.depth += 1;
        Ok(())
    }

    fn leave(&self, key: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active.remove(key);
        state.depth = state.depth.saturating_sub(1);
    }
}

/// Run a declared command, hand it the payload on stdin, and read its verdict
/// from stdout — the shape the imported ecosystems' command hooks already use.
///
/// The deadline is enforced by killing the process, so a handler that ignores
/// it cannot hold a turn open.
pub struct CommandHandler {
    pub program: String,
    pub args: Vec<String>,
    /// Variables the handler inherits. Everything else is dropped, so a
    /// credential in the operator's shell cannot reach a hook.
    pub environment: Vec<(String, String)>,
    /// Bound on what the handler may write back.
    pub max_output_bytes: usize,
}

impl HookHandler for CommandHandler {
    fn run(&self, rule: &HookRule, payload: &Value) -> Result<HandlerResult, HookError> {
        use std::{
            io::{Read, Write},
            process::{Command, Stdio},
        };

        let failed = |message: String| HookError::Handler {
            rule: rule.id.clone(),
            message,
        };
        let mut child = Command::new(&self.program)
            .args(&self.args)
            .env_clear()
            .envs(self.environment.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| failed(error.to_string()))?;
        let body = serde_json::to_vec(payload).map_err(|error| failed(error.to_string()))?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let writer =
            std::thread::spawn(move || stdin.write_all(&body).and_then(|()| stdin.flush()));

        let mut stdout = child.stdout.take().expect("stdout is piped");
        let limit = self.max_output_bytes;
        let reader = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            std::io::Read::by_ref(&mut stdout)
                .take(limit as u64)
                .read_to_end(&mut buffer)
                .map(|_| buffer)
        });

        let deadline = std::time::Instant::now() + rule.timeout;
        loop {
            match child
                .try_wait()
                .map_err(|error| failed(error.to_string()))?
            {
                Some(_) => break,
                None if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(HookError::Timeout {
                        rule: rule.id.clone(),
                        limit: rule.timeout,
                    });
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        // A handler that never reads stdin makes the write fail; that is the
        // handler's choice, not a failure of the hook.
        let _ = writer.join();
        let output = reader
            .join()
            .map_err(|_| failed("the output reader panicked".to_owned()))?
            .map_err(|error| failed(error.to_string()))?;
        decode(rule, &output)
    }
}

/// A handler's stdout as a result. Silence is `Continue`: a hook that only
/// wanted to observe should not have to print anything.
fn decode(rule: &HookRule, output: &[u8]) -> Result<HandlerResult, HookError> {
    let text = std::str::from_utf8(output).map_err(|_| HookError::Handler {
        rule: rule.id.clone(),
        message: "output is not UTF-8".to_owned(),
    })?;
    if text.trim().is_empty() {
        return Ok(HandlerResult::default());
    }
    let value: Value = serde_json::from_str(text.trim()).map_err(|error| HookError::Handler {
        rule: rule.id.clone(),
        message: format!("output is not JSON: {error}"),
    })?;
    let outcome = match value.get("decision").and_then(Value::as_str) {
        Some("deny") => Some(Outcome::Deny(reason(&value))),
        Some("ask") => Some(Outcome::RequireApproval(reason(&value))),
        Some("allow") => Some(Outcome::Allow),
        Some(other) => {
            return Err(HookError::Handler {
                rule: rule.id.clone(),
                message: format!("unknown decision `{other}`"),
            })
        }
        None => None,
    };
    Ok(HandlerResult {
        outcome,
        payload: value.get("payload").cloned(),
        inject: value
            .get("context")
            .and_then(Value::as_str)
            .map(str::to_owned),
        schedule: value.get("schedule").cloned(),
    })
}

fn reason(value: &Value) -> String {
    value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("the hook gave no reason")
        .to_owned()
}

/// Longest a hook may hold a turn when its declaration does not say.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on what one handler may write back.
pub const MAX_HOOK_OUTPUT_BYTES: usize = 64 * 1024;

/// Where hooks are read from, and whether this directory's own files may run.
#[derive(Clone, Debug)]
pub struct Discovery {
    /// The operator's home. Their own files carry their own authority.
    pub home: Option<PathBuf>,
    pub root: PathBuf,
    /// Whether the operator vouched for `root`. Nothing under it executes
    /// until they have.
    pub trusted: bool,
    pub max_depth: u32,
}

/// What one declaration file became.
#[derive(Clone, Debug, Serialize)]
pub struct SourceReport {
    pub path: PathBuf,
    pub origin: String,
    /// `loaded`, `not_loaded`, or `absent`.
    pub status: &'static str,
    pub rules: usize,
    /// Why a file that exists produced nothing, or fewer rules than it names.
    pub notes: Vec<String>,
}

/// An engine built from what is on disk, and an account of where it came from.
pub struct Loaded {
    pub engine: HookEngine,
    pub sources: Vec<SourceReport>,
}

impl Loaded {
    /// Whether anything at all will run. A turn that dispatches into an empty
    /// engine pays for nothing, so the caller can skip it entirely.
    pub fn is_empty(&self) -> bool {
        self.engine.rules.is_empty()
    }
}

/// Build the engine from every source this operator and workspace offer.
///
/// Four places, in authority order:
///
/// * `~/.claude/settings.json` — the operator's own Claude hooks.
/// * `~/.codex/config.toml` — Codex's one lifecycle callback, `notify`.
/// * `~/.arsy/guard.json` — ARSY's own, for an operator using neither.
/// * `<root>/.arsy/guard.json` and `<root>/.claude/settings.json` — the
///   repository's, which run only where the operator vouched for it.
///
/// A source that is missing is not an error: most machines have one of these
/// and not the others. A source that is present and unusable is reported
/// against itself rather than failing the load, because one malformed file
/// must not leave a turn with no hooks at all.
pub fn load(discovery: &Discovery) -> Loaded {
    let mut engine = HookEngine::new(discovery.max_depth);
    let mut sources = Vec::new();
    let workspace_note = || {
        vec![format!(
            "`{}` is not a directory this configuration vouches for, so its own hooks are read and not run",
            discovery.root.display()
        )]
    };

    if let Some(home) = &discovery.home {
        for (path, kind) in [
            (home.join(".claude/settings.json"), Kind::ClaudeSettings),
            (home.join(".codex/config.toml"), Kind::CodexNotify),
            (home.join(".arsy/guard.json"), Kind::ArsyGuard),
        ] {
            sources.push(read_source(
                &mut engine,
                &path,
                kind,
                PolicySource::User,
                true,
                discovery,
            ));
        }
    }
    for (path, kind) in [
        (discovery.root.join(".arsy/guard.json"), Kind::ArsyGuard),
        (
            discovery.root.join(".claude/settings.json"),
            Kind::ClaudeSettings,
        ),
    ] {
        let mut report = read_source(
            &mut engine,
            &path,
            kind,
            PolicySource::Workspace,
            discovery.trusted,
            discovery,
        );
        if report.status == "not_loaded" && !discovery.trusted {
            report.notes = workspace_note();
        }
        sources.push(report);
    }
    Loaded { engine, sources }
}

#[derive(Clone, Copy)]
enum Kind {
    /// A `hooks` object keyed by the external event names.
    ClaudeSettings,
    /// The same shape, under ARSY's own name.
    ArsyGuard,
    /// Codex's `notify`, which is one command on one event.
    CodexNotify,
}

fn read_source(
    engine: &mut HookEngine,
    path: &Path,
    kind: Kind,
    origin: PolicySource,
    execute: bool,
    discovery: &Discovery,
) -> SourceReport {
    let mut report = SourceReport {
        path: path.to_path_buf(),
        origin: origin.to_string(),
        status: "absent",
        rules: 0,
        notes: Vec::new(),
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return report;
    };
    let parsed = match kind {
        Kind::ClaudeSettings | Kind::ArsyGuard => serde_json::from_str::<Value>(&text)
            .map_err(|error| error.to_string())
            .and_then(|value| claude_rules(&value, origin, path, discovery)),
        Kind::CodexNotify => codex_notify(&text, origin, discovery),
    };
    let (rules, notes) = match parsed {
        Ok(loaded) => loaded,
        Err(reason) => {
            report.status = "not_loaded";
            report.notes = vec![reason];
            return report;
        }
    };
    report.notes = notes;
    report.rules = rules.len();
    if rules.is_empty() {
        // A settings file with no hooks in it is not a refusal.
        report.status = if report.notes.is_empty() {
            "absent"
        } else {
            "not_loaded"
        };
        return report;
    }
    if !execute {
        report.status = "not_loaded";
        return report;
    }
    report.status = "loaded";
    for (rule, handler) in rules {
        engine.register(rule, Box::new(handler));
    }
    report
}

type Rules = (Vec<(HookRule, CommandHandler)>, Vec<String>);

/// Read the shape Claude writes, which is also the shape `.arsy/guard.json`
/// uses: one object keyed by event, each holding entries of a matcher and the
/// handlers to run.
///
/// Only `type: "command"` runs. A `prompt`, `agent`, or `http` handler asks
/// for a model call or a network request on the turn's behalf, which is an
/// authority a declaration cannot confer on itself; each is reported against
/// its own file rather than silently dropped.
fn claude_rules(
    settings: &Value,
    origin: PolicySource,
    source: &Path,
    discovery: &Discovery,
) -> Result<Rules, String> {
    let Some(hooks) = settings.get("hooks") else {
        return Ok((Vec::new(), Vec::new()));
    };
    let hooks = hooks
        .as_object()
        .ok_or_else(|| "`hooks` must be an object".to_owned())?;
    let mut rules = Vec::new();
    let mut notes = Vec::new();
    for (external, entries) in hooks {
        let Some(event) = LifecycleEvent::from_external(external) else {
            notes.push(format!("`{external}` is not an event this build has"));
            continue;
        };
        let Some(entries) = entries.as_array() else {
            notes.push(format!("`{external}` must hold an array"));
            continue;
        };
        for (position, entry) in entries.iter().enumerate() {
            let declared = entry.get("matcher").and_then(Value::as_str).unwrap_or("*");
            let handlers = entry.get("hooks").and_then(Value::as_array);
            let Some(handlers) = handlers else {
                notes.push(format!("`{external}[{position}]` names no hooks"));
                continue;
            };
            for (index, handler) in handlers.iter().enumerate() {
                let kind = handler.get("type").and_then(Value::as_str).unwrap_or("");
                if kind != "command" {
                    notes.push(format!(
                        "`{external}[{position}]` handler {index} is `{kind}`, and only `command` runs"
                    ));
                    continue;
                }
                let Some(command) = handler.get("command").and_then(Value::as_str) else {
                    notes.push(format!(
                        "`{external}[{position}]` handler {index} names no command"
                    ));
                    continue;
                };
                let timeout = handler
                    .get("timeout")
                    .and_then(Value::as_u64)
                    .map_or(DEFAULT_HOOK_TIMEOUT, Duration::from_secs);
                // `Edit|Write` is one declaration covering two subjects, and
                // the matcher vocabulary here is a glob rather than an
                // alternation. One rule per alternative says the same thing in
                // the vocabulary the engine has.
                for alternative in declared.split('|') {
                    let matcher = matcher_for(alternative);
                    rules.push((
                        HookRule {
                            id: format!(
                                "{}:{external}[{position}].{index}{}",
                                label(source),
                                if matcher == "*" {
                                    String::new()
                                } else {
                                    format!(":{matcher}")
                                }
                            ),
                            event,
                            matcher,
                            effect: effect_for(event),
                            origin,
                            timeout,
                        },
                        shell_handler(command, discovery),
                    ));
                }
            }
        }
    }
    Ok((rules, notes))
}

/// Codex declares one lifecycle callback: `notify`, an argv run when a turn
/// ends. It is the only hook Codex has, so it is the whole of what an operator
/// who uses Codex has already written.
fn codex_notify(
    config: &str,
    origin: PolicySource,
    discovery: &Discovery,
) -> Result<Rules, String> {
    // A document, not a value: `FromStr` for `toml::Value` reads one value and
    // refuses everything after it.
    let parsed: toml::Table = config
        .parse()
        .map_err(|error: toml::de::Error| error.message().to_owned())?;
    let Some(notify) = parsed.get("notify") else {
        return Ok((Vec::new(), Vec::new()));
    };
    let argv: Vec<String> = notify
        .as_array()
        .ok_or_else(|| "`notify` must be an array of a program and its arguments".to_owned())?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "`notify` holds something that is not a string".to_owned())
        })
        .collect::<Result<_, _>>()?;
    let Some((program, args)) = argv.split_first() else {
        return Ok((Vec::new(), vec!["`notify` is empty".to_owned()]));
    };
    Ok((
        vec![(
            HookRule {
                id: "codex:notify".to_owned(),
                // Codex runs it when the turn ends, and nothing it prints is
                // read back, so it observes.
                event: LifecycleEvent::AfterTurn,
                matcher: "*".to_owned(),
                effect: EffectClass::Observe,
                origin,
                timeout: DEFAULT_HOOK_TIMEOUT,
            },
            CommandHandler {
                program: program.clone(),
                args: args.to_vec(),
                environment: environment(discovery),
                max_output_bytes: MAX_HOOK_OUTPUT_BYTES,
            },
        )],
        Vec::new(),
    ))
}

/// An event that gates registers as a gate; one that reports registers as an
/// observer. The same reading `arsy hook list` has always published.
const fn effect_for(event: LifecycleEvent) -> EffectClass {
    match event.failure_policy() {
        FailurePolicy::FailClosed => EffectClass::Gate,
        FailurePolicy::FailOpen => EffectClass::Observe,
    }
}

/// A declared subject in ARSY's vocabulary. `Bash` is what the ecosystems call
/// what this build calls `process.exec`; an empty matcher means every subject.
fn matcher_for(declared: &str) -> String {
    let declared = declared.trim();
    if declared.is_empty() {
        return "*".to_owned();
    }
    crate::compat::map_tool(declared)
        .unwrap_or(declared)
        .to_owned()
}

/// A command hook is a shell line in every ecosystem that has one — the
/// operator's own settings use `if [ -n "$VAR" ]; then …; fi` — so it is run
/// the way it was written rather than split into an argv it was never meant to
/// be.
fn shell_handler(command: &str, discovery: &Discovery) -> CommandHandler {
    let (program, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("/bin/sh", "-c")
    };
    CommandHandler {
        program: program.to_owned(),
        args: vec![flag.to_owned(), command.to_owned()],
        environment: environment(discovery),
        max_output_bytes: MAX_HOOK_OUTPUT_BYTES,
    }
}

/// What a handler inherits. Named rather than inherited wholesale, because the
/// operator's shell is where credentials live and a hook is a program a
/// declaration chose.
fn environment(discovery: &Discovery) -> Vec<(String, String)> {
    let mut environment = vec![(
        "ARSY_WORKSPACE".to_owned(),
        discovery.root.display().to_string(),
    )];
    for name in ["PATH", "HOME", "LANG", "TMPDIR", "SystemRoot", "PATHEXT"] {
        if let Some(value) = std::env::var_os(name).and_then(|value| value.into_string().ok()) {
            environment.push((name.to_owned(), value));
        }
    }
    environment
}

/// A source's short name, so a rule id says which file asked for it.
fn label(source: &Path) -> String {
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("hooks");
    match source
        .parent()
        .and_then(Path::file_name)
        .and_then(|parent| parent.to_str())
    {
        Some(parent) => format!("{parent}/{name}"),
        None => name.to_owned(),
    }
}

/// Render one rule the way `arsy hook list` reports it.
pub fn describe(rule: &HookRule) -> Value {
    json!({
        "id": rule.id,
        "event": rule.event.as_str(),
        "matcher": rule.matcher,
        "effect": rule.effect,
        "origin": rule.origin.to_string(),
        "may_grant": rule.origin.may_grant(),
        "timeout_ms": rule.timeout.as_millis() as u64,
        "on_failure": rule.event.failure_policy(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        for (relative, body) in files {
            let path = directory.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        directory
    }

    const CLAUDE: &str = r#"{"hooks": {
        "PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "audit", "timeout": 3}]},
            {"matcher": "Edit|Write", "hooks": [{"type": "command", "command": "check"}]},
            {"matcher": "Bash", "hooks": [{"type": "prompt", "prompt": "think about it"}]}
        ],
        "Stop": [{"hooks": [{"type": "command", "command": "done"}]}],
        "Notification": [{"hooks": [{"type": "command", "command": "ping"}]}]
    }}"#;

    fn discovery(home: &Path, root: &Path, trusted: bool) -> Discovery {
        Discovery {
            home: Some(home.to_path_buf()),
            root: root.to_path_buf(),
            trusted,
            max_depth: 4,
        }
    }

    /// The operator's own files carry the operator's authority; a repository's
    /// carry the repository's, and run only where the operator said so.
    #[test]
    fn the_operators_own_hooks_run_and_a_repositorys_wait_to_be_vouched_for() {
        let home = home_with(&[(".claude/settings.json", CLAUDE)]);
        let workspace = home_with(&[(".claude/settings.json", CLAUDE)]);

        let untrusted = load(&discovery(home.path(), workspace.path(), false));
        let mine: Vec<&HookRule> = untrusted
            .engine
            .rules()
            .filter(|rule| rule.origin == PolicySource::User)
            .collect();
        assert!(!mine.is_empty(), "the operator's own hooks are loaded");
        assert!(
            untrusted
                .engine
                .rules()
                .all(|rule| rule.origin != PolicySource::Workspace),
            "an unvouched repository runs none of its own hooks"
        );
        let repository = untrusted
            .sources
            .iter()
            .find(|source| source.origin == "workspace" && source.rules > 0)
            .expect("the repository's file is still read and reported");
        assert_eq!(repository.status, "not_loaded");
        assert!(
            repository.notes[0].contains("vouches for"),
            "{:?}",
            repository.notes
        );

        // Vouched for, the same file runs — and still cannot grant, because
        // its origin is the repository's.
        let trusted = load(&discovery(home.path(), workspace.path(), true));
        assert!(trusted
            .engine
            .rules()
            .any(|rule| rule.origin == PolicySource::Workspace));
        assert!(!PolicySource::Workspace.may_grant());
    }

    #[test]
    fn a_declaration_becomes_the_rules_it_names_and_reports_what_it_cannot() {
        let home = home_with(&[(".claude/settings.json", CLAUDE)]);
        let workspace = tempfile::tempdir().unwrap();

        let loaded = load(&discovery(home.path(), workspace.path(), false));
        let rules: Vec<&HookRule> = loaded.engine.rules().collect();

        // `Bash` is this build's `process.exec`, and `Edit|Write` is two
        // subjects in one declaration, so it is two rules.
        let subjects: BTreeSet<&str> = rules
            .iter()
            .filter(|rule| rule.event == LifecycleEvent::BeforeOperation)
            .map(|rule| rule.matcher.as_str())
            .collect();
        assert_eq!(
            subjects,
            BTreeSet::from(["process.exec", "Edit", "Write"]),
            "{subjects:?}"
        );
        // A declared timeout is the rule's; one that says nothing gets the
        // default rather than none.
        let audit = rules
            .iter()
            .find(|rule| rule.matcher == "process.exec")
            .unwrap();
        assert_eq!(audit.timeout, Duration::from_secs(3));
        assert_eq!(
            rules
                .iter()
                .find(|rule| rule.event == LifecycleEvent::AfterTurn)
                .unwrap()
                .timeout,
            DEFAULT_HOOK_TIMEOUT
        );
        // An event that gates registers as a gate; one that reports does not.
        assert_eq!(audit.effect, EffectClass::Gate);
        assert_eq!(
            rules
                .iter()
                .find(|rule| rule.event == LifecycleEvent::AfterTurn)
                .unwrap()
                .effect,
            EffectClass::Observe
        );

        let report = loaded
            .sources
            .iter()
            .find(|source| source.path.ends_with(".claude/settings.json"))
            .unwrap();
        assert_eq!(report.status, "loaded");
        // What it could not take is said against the file, not dropped.
        assert!(
            report.notes.iter().any(|note| note.contains("`prompt`")),
            "{:?}",
            report.notes
        );
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("Notification")),
            "{:?}",
            report.notes
        );
    }

    /// Codex has one lifecycle callback. An operator who uses Codex has
    /// already written it, and it is the whole of what there is to adopt.
    #[test]
    fn codex_notify_is_the_turn_ending_hook_it_already_is() {
        let home = home_with(&[(
            ".codex/config.toml",
            "model = \"gpt-5\"\nnotify = [\"/opt/notify\", \"turn-ended\"]\n",
        )]);
        let workspace = tempfile::tempdir().unwrap();

        let loaded = load(&discovery(home.path(), workspace.path(), false));

        let rule = loaded
            .engine
            .rules()
            .find(|rule| rule.id == "codex:notify")
            .expect("notify became a rule");
        assert_eq!(rule.event, LifecycleEvent::AfterTurn);
        assert_eq!(rule.matcher, "*");
        assert_eq!(rule.origin, PolicySource::User);
        // A Codex config without `notify` is simply a config without a hook.
        let bare = home_with(&[(".codex/config.toml", "model = \"gpt-5\"\n")]);
        let none = load(&discovery(bare.path(), workspace.path(), false));
        assert!(none.is_empty());
    }

    /// An operator using neither ecosystem writes the same shape under ARSY's
    /// own name, so a hook can be moved between them unchanged.
    #[test]
    fn arsy_reads_its_own_file_in_the_shape_the_others_use() {
        let home = home_with(&[(
            ".arsy/guard.json",
            r#"{"hooks": {"PreToolUse": [{"matcher": "process.exec",
                "hooks": [{"type": "command", "command": "mine"}]}]}}"#,
        )]);
        let workspace = tempfile::tempdir().unwrap();

        let loaded = load(&discovery(home.path(), workspace.path(), false));

        assert_eq!(loaded.engine.rules().count(), 1);
        assert_eq!(
            loaded.engine.rules().next().unwrap().event,
            LifecycleEvent::BeforeOperation
        );
    }

    /// One unreadable file must not leave a turn with no hooks at all.
    #[test]
    fn a_malformed_source_is_reported_against_itself_and_the_others_still_load() {
        let home = home_with(&[
            (".claude/settings.json", "{not json"),
            (
                ".arsy/guard.json",
                r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "ok"}]}]}}"#,
            ),
        ]);
        let workspace = tempfile::tempdir().unwrap();

        let loaded = load(&discovery(home.path(), workspace.path(), false));

        assert_eq!(loaded.engine.rules().count(), 1, "the good file still ran");
        let broken = loaded
            .sources
            .iter()
            .find(|source| source.path.ends_with(".claude/settings.json"))
            .unwrap();
        assert_eq!(broken.status, "not_loaded");
        assert_eq!(broken.rules, 0);
        assert!(!broken.notes.is_empty(), "it says why");
        // A machine with none of these files is not an error either.
        let empty = tempfile::tempdir().unwrap();
        assert!(load(&discovery(empty.path(), workspace.path(), true)).is_empty());
    }

    /// A handler that returns a scripted result, and records that it ran.
    struct Scripted(Result<HandlerResult, HookError>);

    impl HookHandler for Scripted {
        fn run(&self, _rule: &HookRule, _payload: &Value) -> Result<HandlerResult, HookError> {
            self.0.clone()
        }
    }

    /// A handler that re-raises its own event, which is what a loop looks like.
    struct Reraise(std::sync::Weak<HookEngine>);

    impl HookHandler for Reraise {
        fn run(&self, rule: &HookRule, _payload: &Value) -> Result<HandlerResult, HookError> {
            let engine = self.0.upgrade().expect("the engine outlives its handlers");
            engine.dispatch(rule.event, "turn", json!({}))?;
            Ok(HandlerResult::default())
        }
    }

    fn rule(
        id: &str,
        event: LifecycleEvent,
        effect: EffectClass,
        origin: PolicySource,
    ) -> HookRule {
        HookRule {
            id: id.to_owned(),
            event,
            matcher: "*".to_owned(),
            effect,
            origin,
            timeout: Duration::from_millis(500),
        }
    }

    #[test]
    fn matchers_select_the_subject_and_nothing_else() {
        let mut rule = rule(
            "a",
            LifecycleEvent::BeforeOperation,
            EffectClass::Observe,
            PolicySource::User,
        );
        assert!(rule.matches("process.exec"));
        rule.matcher = "process.*".to_owned();
        assert!(rule.matches("process.exec"));
        assert!(!rule.matches("git.status"));
        rule.matcher = "*.exec".to_owned();
        assert!(rule.matches("process.exec"));
        assert!(!rule.matches("process.signal"));
        rule.matcher = "git.status".to_owned();
        assert!(rule.matches("git.status"));
        assert!(!rule.matches("git.statuses"));
    }

    #[test]
    fn a_workspace_hook_may_deny_but_never_grant() {
        let mut engine = HookEngine::new(4);
        engine.register(
            rule(
                "repo-allow",
                LifecycleEvent::BeforeOperation,
                EffectClass::Gate,
                PolicySource::Workspace,
            ),
            Box::new(Scripted(Ok(HandlerResult {
                outcome: Some(Outcome::Allow),
                ..HandlerResult::default()
            }))),
        );
        let dispatched = engine
            .dispatch(LifecycleEvent::BeforeOperation, "process.exec", json!({}))
            .unwrap();
        assert_eq!(dispatched.outcome, Outcome::Continue);
        assert_eq!(dispatched.diagnostics.len(), 1);
        assert!(dispatched.diagnostics[0].contains("cannot grant authority"));

        // The same result from an origin that may grant is a grant — which is
        // what makes the refusal above a refusal rather than an outcome no
        // hook could ever reach.
        let mut engine = HookEngine::new(4);
        engine.register(
            rule(
                "user-allow",
                LifecycleEvent::BeforeOperation,
                EffectClass::Gate,
                PolicySource::User,
            ),
            Box::new(Scripted(Ok(HandlerResult {
                outcome: Some(Outcome::Allow),
                ..HandlerResult::default()
            }))),
        );
        let granted = engine
            .dispatch(LifecycleEvent::BeforeOperation, "process.exec", json!({}))
            .unwrap();
        assert_eq!(granted.outcome, Outcome::Allow);
        assert!(granted.diagnostics.is_empty());

        // A grant does not overrule another hook's objection.
        let mut engine = HookEngine::new(4);
        engine.register(
            rule(
                "ask",
                LifecycleEvent::BeforeOperation,
                EffectClass::Gate,
                PolicySource::User,
            ),
            Box::new(Scripted(Ok(HandlerResult {
                outcome: Some(Outcome::RequireApproval("check first".to_owned())),
                ..HandlerResult::default()
            }))),
        );
        engine.register(
            rule(
                "user-allow",
                LifecycleEvent::BeforeOperation,
                EffectClass::Gate,
                PolicySource::User,
            ),
            Box::new(Scripted(Ok(HandlerResult {
                outcome: Some(Outcome::Allow),
                ..HandlerResult::default()
            }))),
        );
        let contested = engine
            .dispatch(LifecycleEvent::BeforeOperation, "process.exec", json!({}))
            .unwrap();
        assert_eq!(
            contested.outcome,
            Outcome::RequireApproval("check first".to_owned())
        );

        let mut engine = HookEngine::new(4);
        engine.register(
            rule(
                "repo-deny",
                LifecycleEvent::BeforeOperation,
                EffectClass::Gate,
                PolicySource::Workspace,
            ),
            Box::new(Scripted(Ok(HandlerResult {
                outcome: Some(Outcome::Deny("not here".to_owned())),
                ..HandlerResult::default()
            }))),
        );
        assert_eq!(
            engine
                .dispatch(LifecycleEvent::BeforeOperation, "process.exec", json!({}))
                .unwrap()
                .outcome,
            Outcome::Deny("not here".to_owned()),
            "restricting needs no authority"
        );
    }

    #[test]
    fn a_denial_stops_the_chain_and_the_payload_threads_through_it() {
        let mut engine = HookEngine::new(4);
        engine.register(
            rule(
                "rewrite",
                LifecycleEvent::BeforeTurn,
                EffectClass::Transform,
                PolicySource::User,
            ),
            Box::new(Scripted(Ok(HandlerResult {
                payload: Some(json!({"prompt": "redacted"})),
                inject: Some("a note".to_owned()),
                ..HandlerResult::default()
            }))),
        );
        engine.register(
            rule(
                "observe-only",
                LifecycleEvent::BeforeTurn,
                EffectClass::Observe,
                PolicySource::User,
            ),
            Box::new(Scripted(Ok(HandlerResult {
                payload: Some(json!({"prompt": "sneaky"})),
                ..HandlerResult::default()
            }))),
        );
        engine.register(
            rule(
                "gate",
                LifecycleEvent::BeforeTurn,
                EffectClass::Gate,
                PolicySource::User,
            ),
            Box::new(Scripted(Ok(HandlerResult {
                outcome: Some(Outcome::Deny("nope".to_owned())),
                ..HandlerResult::default()
            }))),
        );
        engine.register(
            rule(
                "never-runs",
                LifecycleEvent::BeforeTurn,
                EffectClass::Observe,
                PolicySource::User,
            ),
            Box::new(Scripted(Err(HookError::Handler {
                rule: "never-runs".to_owned(),
                message: "should not be reached".to_owned(),
            }))),
        );

        let dispatched = engine
            .dispatch(
                LifecycleEvent::BeforeTurn,
                "turn",
                json!({"prompt": "hello"}),
            )
            .unwrap();
        assert_eq!(dispatched.outcome, Outcome::Deny("nope".to_owned()));
        assert_eq!(dispatched.payload, json!({"prompt": "redacted"}));
        assert_eq!(
            dispatched.injected,
            vec![("rewrite".to_owned(), "a note".to_owned())]
        );
        assert_eq!(dispatched.ran, ["rewrite", "observe-only", "gate"]);
        assert!(
            dispatched
                .diagnostics
                .iter()
                .any(|note| note.contains("observe")),
            "a rewrite from an observe-only rule is refused and reported: {:?}",
            dispatched.diagnostics
        );
    }

    #[test]
    fn failure_is_closed_on_a_gate_and_open_on_a_report() {
        let broken = || {
            Box::new(Scripted(Err(HookError::Timeout {
                rule: "slow".to_owned(),
                limit: Duration::from_millis(1),
            }))) as Box<dyn HookHandler>
        };
        let mut engine = HookEngine::new(4);
        engine.register(
            rule(
                "slow",
                LifecycleEvent::BeforeOperation,
                EffectClass::Gate,
                PolicySource::User,
            ),
            broken(),
        );
        let dispatched = engine
            .dispatch(LifecycleEvent::BeforeOperation, "process.exec", json!({}))
            .unwrap();
        assert!(
            matches!(dispatched.outcome, Outcome::Deny(_)),
            "a gate that cannot run has approved nothing"
        );

        let mut engine = HookEngine::new(4);
        engine.register(
            rule(
                "slow",
                LifecycleEvent::AfterOperation,
                EffectClass::Observe,
                PolicySource::User,
            ),
            broken(),
        );
        let dispatched = engine
            .dispatch(LifecycleEvent::AfterOperation, "process.exec", json!({}))
            .unwrap();
        assert_eq!(dispatched.outcome, Outcome::Continue);
        assert_eq!(dispatched.diagnostics.len(), 1, "reported, not silent");
    }

    #[test]
    fn a_hook_that_reraises_its_own_event_is_refused_rather_than_looping() {
        // The handler needs the engine it is registered on, so the engine is
        // built inside its own `Arc`.
        let engine = std::sync::Arc::new_cyclic(|weak: &std::sync::Weak<HookEngine>| {
            let mut engine = HookEngine::new(8);
            engine.register(
                rule(
                    "reraise",
                    LifecycleEvent::BeforeTurn,
                    EffectClass::Observe,
                    PolicySource::User,
                ),
                Box::new(Reraise(weak.clone())),
            );
            engine
        });
        let dispatched = engine
            .dispatch(LifecycleEvent::BeforeTurn, "turn", json!({}))
            .unwrap();
        assert!(
            matches!(dispatched.outcome, Outcome::Deny(_)),
            "before_turn fails closed, so a loop denies rather than recursing"
        );
        assert_eq!(dispatched.diagnostics.len(), 1);
        assert!(
            dispatched.diagnostics[0].contains("already dispatching"),
            "{:?}",
            dispatched.diagnostics
        );
        // The guard is released afterwards, so the engine still works.
        assert!(engine
            .dispatch(LifecycleEvent::AfterTurn, "turn", json!({}))
            .is_ok());
    }

    #[test]
    fn depth_bounds_how_far_hooks_may_nest() {
        let engine = HookEngine::new(2);
        engine
            .enter(LifecycleEvent::BeforeTurn, "before_turn:turn")
            .unwrap();
        // A different subject still nests, up to the allowance.
        assert!(engine
            .dispatch(LifecycleEvent::AfterTurn, "turn", json!({}))
            .is_ok());
        engine.leave("before_turn:turn");

        let shallow = HookEngine::new(1);
        shallow.enter(LifecycleEvent::BeforeTurn, "a").unwrap();
        assert_eq!(
            shallow
                .dispatch(LifecycleEvent::AfterTurn, "b", json!({}))
                .unwrap_err(),
            HookError::RecursionLimit(1)
        );
        shallow.leave("a");
        assert!(shallow
            .dispatch(LifecycleEvent::AfterTurn, "b", json!({}))
            .is_ok());
    }

    #[test]
    fn only_one_follow_up_may_be_scheduled() {
        let mut engine = HookEngine::new(4);
        for id in ["first", "second"] {
            engine.register(
                rule(
                    id,
                    LifecycleEvent::AfterTurn,
                    EffectClass::Observe,
                    PolicySource::User,
                ),
                Box::new(Scripted(Ok(HandlerResult {
                    schedule: Some(json!({"task": id})),
                    ..HandlerResult::default()
                }))),
            );
        }
        assert_eq!(
            engine
                .dispatch(LifecycleEvent::AfterTurn, "turn", json!({}))
                .unwrap_err(),
            HookError::TooManyFollowUps("second".to_owned())
        );
    }

    /// Unix-gated like the other tests here that need a POSIX shell: the
    /// handler protocol is the same everywhere, but a fixture that speaks it
    /// is not.
    #[cfg(unix)]
    #[test]
    fn a_command_handler_decodes_a_verdict_and_is_bounded_by_its_deadline() {
        let rule = HookRule {
            timeout: Duration::from_millis(1_500),
            ..rule(
                "cmd",
                LifecycleEvent::BeforeOperation,
                EffectClass::Gate,
                PolicySource::User,
            )
        };
        let handler = CommandHandler {
            program: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                // Consume the payload, then answer: a handler that never reads
                // stdin is a different case, covered by the deadline below.
                r#"cat >/dev/null; printf '{"decision":"deny","reason":"policy"}\n'"#.to_owned(),
            ],
            environment: Vec::new(),
            max_output_bytes: 4096,
        };
        assert_eq!(
            handler
                .run(&rule, &json!({"kind": "process.exec"}))
                .unwrap(),
            HandlerResult {
                outcome: Some(Outcome::Deny("policy".to_owned())),
                ..HandlerResult::default()
            }
        );

        let slow = CommandHandler {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), "sleep 30".to_owned()],
            environment: Vec::new(),
            max_output_bytes: 4096,
        };
        // The error itself proves the deadline fired: a handler that ran to
        // completion would return a verdict, not a Timeout.
        assert_eq!(
            slow.run(&rule, &json!({})).unwrap_err(),
            HookError::Timeout {
                rule: "cmd".to_owned(),
                limit: Duration::from_millis(1_500),
            }
        );
    }

    #[test]
    fn silence_is_continue_and_nonsense_is_an_error() {
        let rule = rule(
            "cmd",
            LifecycleEvent::AfterTurn,
            EffectClass::Observe,
            PolicySource::User,
        );
        assert_eq!(decode(&rule, b"  \n").unwrap(), HandlerResult::default());
        assert!(decode(&rule, b"not json").is_err());
        assert!(decode(&rule, br#"{"decision": "maybe"}"#).is_err());
        assert_eq!(
            decode(&rule, br#"{"context": "note"}"#).unwrap().inject,
            Some("note".to_owned())
        );
    }
}
