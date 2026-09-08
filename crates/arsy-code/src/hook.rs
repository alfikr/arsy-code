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
use std::{collections::BTreeSet, fmt, sync::Mutex, time::Duration};

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
            if outcome.rank() < dispatch.outcome.rank() {
                dispatch.outcome = outcome;
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
        let started = std::time::Instant::now();
        assert_eq!(
            slow.run(&rule, &json!({})).unwrap_err(),
            HookError::Timeout {
                rule: "cmd".to_owned(),
                limit: Duration::from_millis(1_500),
            }
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the deadline must kill the handler, not wait it out"
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
