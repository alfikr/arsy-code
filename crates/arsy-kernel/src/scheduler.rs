//! Admission and lifecycle for attempts that run while their parent keeps
//! working.
//!
//! # Why this is not a second task database
//!
//! Everything durable still lives in the session's event stream through
//! [`TaskGraph`]. The scheduler owns only what cannot be replayed: which slots
//! are occupied right now, and which in-process holder to interrupt when an
//! attempt is cancelled. A restart rebuilds the graph and finds the slots
//! empty, which is true — nothing is running in a process that no longer
//! exists.
//!
//! # What admission bounds
//!
//! Concurrency, not spend. A task's allowance is held against its parent by
//! the graph before anything starts, so admission does not need to re-decide
//! budget; it decides how many attempts may be in flight against a provider, a
//! model, a workspace, or the machine at once. Those are named as opaque keys
//! rather than fields, because the list of things worth bounding grows and a
//! counter per name is the whole mechanism either way.

use crate::{
    domain::{AttemptId, TaskId},
    orchestration::{
        AttemptOutcome, AttemptRequest, AttemptState, GraphError, JoinPolicy, TaskGraph, TaskState,
    },
};
use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

/// A stop signal shared with whoever is running an attempt.
///
/// Cancellation has to reach a model stream mid-token and a child loop between
/// rounds, and neither of those can be reached by appending an event. So the
/// durable request is recorded in the graph and this is what the holder polls.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// How many attempts may occupy each named slot at once.
#[derive(Debug, Default)]
pub struct Admission {
    limits: BTreeMap<String, u64>,
    in_use: BTreeMap<String, u64>,
}

impl Admission {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bound one named dimension. A key with no limit is unbounded, so a
    /// caller only has to name what it actually wants to restrict.
    #[must_use]
    pub fn limit(mut self, key: impl Into<String>, most: u64) -> Self {
        self.limits.insert(key.into(), most);
        self
    }

    /// Take every key or none.
    ///
    /// All-or-nothing because a partial hold is a leak: an attempt that got
    /// its provider slot and not its workspace slot would either run outside
    /// its bound or have to give back a slot someone else has since taken.
    fn try_admit(&mut self, keys: &[String]) -> Result<(), String> {
        if let Some(full) = keys.iter().find(|key| {
            self.limits
                .get(*key)
                .is_some_and(|most| self.in_use.get(*key).copied().unwrap_or(0) >= *most)
        }) {
            return Err(full.clone());
        }
        for key in keys {
            *self.in_use.entry(key.clone()).or_insert(0) += 1;
        }
        Ok(())
    }

    fn release(&mut self, keys: &[String]) {
        for key in keys {
            if let Some(count) = self.in_use.get_mut(key) {
                *count = count.saturating_sub(1);
            }
        }
    }

    /// How many attempts hold this key right now.
    pub fn in_use(&self, key: &str) -> u64 {
        self.in_use.get(key).copied().unwrap_or(0)
    }
}

/// When a failed attempt may be tried again.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub max_attempts: usize,
    pub base_backoff_ms: u64,
}

impl Default for RetryPolicy {
    /// Three tries, then it is not a transient fault. A second of backoff so a
    /// provider that just refused is not asked again in the same breath.
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_backoff_ms: 1_000,
        }
    }
}

impl RetryPolicy {
    /// Exponential in the number of tries already made, capped so a long-lived
    /// session cannot shift a delay into the far future.
    pub fn backoff_ms(self, attempts_made: usize) -> u64 {
        self.base_backoff_ms
            .saturating_mul(1u64 << attempts_made.min(BACKOFF_SHIFT_CAP))
    }
}

/// What a successful admission hands back.
pub struct Admitted {
    pub attempt: AttemptId,
    pub cancel: CancelToken,
}

/// The one local scheduler over a session's task graph.
pub struct Scheduler {
    graph: TaskGraph,
    admission: Admission,
    retry: RetryPolicy,
    /// Slots each in-flight attempt holds, so finishing returns exactly what
    /// starting took.
    held: BTreeMap<AttemptId, Vec<String>>,
    cancels: BTreeMap<AttemptId, CancelToken>,
}

impl Scheduler {
    pub fn new(graph: TaskGraph, admission: Admission, retry: RetryPolicy) -> Self {
        Self {
            graph,
            admission,
            retry,
            held: BTreeMap::new(),
            cancels: BTreeMap::new(),
        }
    }

    pub fn graph(&self) -> &TaskGraph {
        &self.graph
    }

    pub fn graph_mut(&mut self) -> &mut TaskGraph {
        &mut self.graph
    }

    /// Start one attempt and return immediately.
    ///
    /// The caller gets an attempt id and a stop signal, not an answer: that is
    /// the whole point of the phase. Whoever runs the attempt is expected to
    /// call [`running`](Self::running) when it picks the work up and
    /// [`finish`](Self::finish) when it is done.
    pub fn start(
        &mut self,
        task: TaskId,
        request: &AttemptRequest,
        slots: Vec<String>,
    ) -> Result<Admitted, SchedulerError> {
        self.admission
            .try_admit(&slots)
            .map_err(SchedulerError::Deferred)?;
        let attempt = match self.graph.start_attempt(task, request) {
            Ok(attempt) => attempt,
            Err(error) => {
                // The slot was taken on the way in; a start that never
                // happened must not keep it.
                self.admission.release(&slots);
                return Err(SchedulerError::Graph(error));
            }
        };
        let cancel = CancelToken::new();
        self.held.insert(attempt, slots);
        self.cancels.insert(attempt, cancel.clone());
        Ok(Admitted { attempt, cancel })
    }

    /// Record that the holder has actually picked the attempt up.
    pub fn running(&mut self, attempt: AttemptId) -> Result<(), GraphError> {
        self.graph.advance_attempt(attempt, AttemptState::Running)
    }

    /// End an attempt and give its slots back.
    ///
    /// The slots are released whatever the graph says, because they describe
    /// this process rather than the record: an outcome the graph refuses as
    /// fenced still means nothing of ours is running under that attempt.
    pub fn finish(
        &mut self,
        attempt: AttemptId,
        outcome: &AttemptOutcome,
    ) -> Result<(), GraphError> {
        let result = self.graph.finish_attempt(attempt, outcome);
        if let Some(slots) = self.held.remove(&attempt) {
            self.admission.release(&slots);
        }
        self.cancels.remove(&attempt);
        // A task that just ended may be the reason other tasks can never run.
        self.graph.block_unreachable()?;
        result
    }

    /// Ask an attempt to stop, durably and then in this process.
    ///
    /// Recorded first: a cancellation that only flipped a flag would be lost
    /// with the process, and the attempt would come back looking resumable.
    /// The attempt is not ended here — its holder still has to report what it
    /// produced, so cancelling never discards evidence.
    pub fn cancel(
        &mut self,
        attempt: AttemptId,
        reason: impl Into<String>,
    ) -> Result<(), GraphError> {
        self.graph.request_cancel(attempt, reason)?;
        if let Some(token) = self.cancels.get(&attempt) {
            token.cancel();
        }
        Ok(())
    }

    /// Whether a set of tasks has reached what the join asked for.
    pub fn joined(&self, tasks: &[TaskId], policy: JoinPolicy) -> bool {
        policy.satisfied(
            tasks
                .iter()
                .filter_map(|id| self.graph.node(*id))
                .map(|node| node.state),
        )
    }

    /// Tasks whose last attempt failed retryably and whose backoff has passed.
    ///
    /// Reported rather than restarted, because starting one needs a request —
    /// a model decision, a base revision — that only the caller can build.
    pub fn retryable(&self, now_ms: u64) -> Vec<TaskId> {
        self.graph
            .tasks()
            .filter(|node| node.state == TaskState::Failed)
            .filter(|node| node.runtime.attempts.len() < self.retry.max_attempts)
            .filter(|node| {
                node.runtime
                    .attempts
                    .last()
                    .and_then(|attempt| self.graph.attempt(*attempt))
                    .is_some_and(|attempt| {
                        attempt.retryable == crate::orchestration::Retryability::Retryable
                            && attempt.ended_at_ms.is_some_and(|ended| {
                                ended.saturating_add(
                                    self.retry.backoff_ms(node.runtime.attempts.len()),
                                ) <= now_ms
                            })
                    })
            })
            .map(|node| node.id)
            .collect()
    }

    /// Start a fresh attempt for a task whose last one failed retryably.
    pub fn retry(
        &mut self,
        task: TaskId,
        request: &AttemptRequest,
        slots: Vec<String>,
    ) -> Result<Admitted, SchedulerError> {
        self.admission
            .try_admit(&slots)
            .map_err(SchedulerError::Deferred)?;
        let attempt = match self.graph.retry(task, request, self.retry.max_attempts) {
            Ok(attempt) => attempt,
            Err(error) => {
                self.admission.release(&slots);
                return Err(SchedulerError::Graph(error));
            }
        };
        let cancel = CancelToken::new();
        self.held.insert(attempt, slots);
        self.cancels.insert(attempt, cancel.clone());
        Ok(Admitted { attempt, cancel })
    }

    pub fn admission(&self) -> &Admission {
        &self.admission
    }
}

/// Doubling stops here: past sixteen the delay is longer than any session.
const BACKOFF_SHIFT_CAP: usize = 16;

#[derive(Debug)]
pub enum SchedulerError {
    /// No slot free for the named key. The task stays where it was; asking
    /// again later is the whole remedy.
    Deferred(String),
    Graph(GraphError),
}

impl fmt::Display for SchedulerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deferred(key) => write!(formatter, "no free slot for {key}"),
            Self::Graph(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SchedulerError {}

impl From<GraphError> for SchedulerError {
    fn from(value: GraphError) -> Self {
        Self::Graph(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{AgentId, Principal, SessionId},
        event::MemoryEventStore,
        orchestration::{Budget, Retryability, TaskNode, TaskRuntime, WorkspaceRequirement},
    };
    use serde_json::json;

    fn scheduler(limit: u64) -> (Scheduler, TaskId) {
        let store = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph =
            TaskGraph::new(store, session, Principal::User("operator".into())).expect("graph");
        let root = TaskId::new();
        graph
            .add(TaskNode {
                id: root,
                goal: "root".into(),
                dependencies: Vec::new(),
                assignee: None,
                required_output: "evidence".into(),
                workspace: WorkspaceRequirement::ReadOnlySnapshot,
                budget: Budget {
                    tokens: 1_000,
                    cost_micros: 1_000,
                    wall_ms: 1_000,
                },
                authority: Vec::new(),
                state: TaskState::Pending,
                lease_expires_at_ms: None,
                runtime: TaskRuntime::default(),
            })
            .expect("root");
        graph.ready().expect("ready");
        (
            Scheduler::new(
                graph,
                Admission::new().limit("provider:test", limit),
                RetryPolicy {
                    max_attempts: 3,
                    base_backoff_ms: 10,
                },
            ),
            root,
        )
    }

    fn request() -> AttemptRequest {
        AttemptRequest {
            role: "worker".into(),
            assignee: AgentId::new(),
            model: None,
            base_revision: None,
            started_at_ms: 0,
            lease_expires_at_ms: 10_000,
        }
    }

    fn child(parent: TaskId, graph: &mut TaskGraph) -> TaskId {
        let id = TaskId::new();
        graph
            .add(TaskNode {
                id,
                goal: "child".into(),
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
                runtime: TaskRuntime {
                    parent: Some(parent),
                    ..TaskRuntime::default()
                },
            })
            .expect("child");
        graph.ready().expect("ready");
        id
    }

    #[test]
    fn starting_an_attempt_returns_before_it_has_run() {
        let (mut scheduler, root) = scheduler(2);
        let admitted = scheduler.start(root, &request(), vec![]).expect("start");
        // The whole claim of the phase: there is an id to ask about, and the
        // attempt has not produced anything yet.
        assert_eq!(
            scheduler.graph().attempt(admitted.attempt).map(|a| a.state),
            Some(AttemptState::Starting)
        );
        assert!(scheduler
            .graph()
            .attempt(admitted.attempt)
            .is_some_and(|attempt| attempt.result.is_none()));
    }

    #[test]
    fn a_full_slot_defers_rather_than_queueing_behind_itself() {
        let (mut scheduler, root) = scheduler(1);
        let first = child(root, scheduler.graph_mut());
        let second = child(root, scheduler.graph_mut());
        scheduler
            .start(first, &request(), vec!["provider:test".into()])
            .expect("first");
        let refused = scheduler.start(second, &request(), vec!["provider:test".into()]);
        assert!(matches!(refused, Err(SchedulerError::Deferred(key)) if key == "provider:test"));
        // The refusal must not have consumed the task's turn: it is still
        // startable once the slot frees.
        assert_eq!(
            scheduler.graph().node(second).map(|node| node.state),
            Some(TaskState::Ready)
        );
    }

    #[test]
    fn finishing_returns_the_slot_it_took() {
        let (mut scheduler, root) = scheduler(1);
        let task = child(root, scheduler.graph_mut());
        let admitted = scheduler
            .start(task, &request(), vec!["provider:test".into()])
            .expect("start");
        assert_eq!(scheduler.admission().in_use("provider:test"), 1);
        scheduler
            .finish(
                admitted.attempt,
                &AttemptOutcome::completed(Budget::default(), json!("answered"), 5),
            )
            .expect("finish");
        assert_eq!(scheduler.admission().in_use("provider:test"), 0);
    }

    #[test]
    fn a_refused_start_does_not_keep_the_slot_it_reserved() {
        let (mut scheduler, root) = scheduler(1);
        let task = child(root, scheduler.graph_mut());
        scheduler
            .graph_mut()
            .cancel(task, "operator")
            .expect("cancel");
        // Cancelled, so the graph refuses the attempt after admission took
        // the slot. A leak here would wedge the only slot forever.
        assert!(scheduler
            .start(task, &request(), vec!["provider:test".into()])
            .is_err());
        assert_eq!(scheduler.admission().in_use("provider:test"), 0);
    }

    #[test]
    fn cancelling_signals_the_holder_without_discarding_its_evidence() {
        let (mut scheduler, root) = scheduler(2);
        let task = child(root, scheduler.graph_mut());
        let admitted = scheduler.start(task, &request(), vec![]).expect("start");
        scheduler.running(admitted.attempt).expect("running");
        scheduler
            .cancel(admitted.attempt, "operator changed their mind")
            .expect("cancel");
        assert!(admitted.cancel.is_cancelled());
        assert_eq!(
            scheduler.graph().attempt(admitted.attempt).map(|a| a.state),
            Some(AttemptState::Cancelling)
        );
        // The holder still reports what it produced.
        scheduler
            .finish(
                admitted.attempt,
                &AttemptOutcome {
                    state: AttemptState::Cancelled,
                    used: Budget::default(),
                    reason: Some("stopped".into()),
                    retryable: Retryability::NotRetryable,
                    result: Some(json!("partial notes")),
                    evidence: vec!["artifact-1".into()],
                    ended_at_ms: 9,
                },
            )
            .expect("finish");
        let attempt = scheduler
            .graph()
            .attempt(admitted.attempt)
            .expect("attempt");
        assert_eq!(attempt.state, AttemptState::Cancelled);
        assert_eq!(attempt.evidence, vec!["artifact-1".to_owned()]);
    }

    #[test]
    fn only_a_retryable_failure_comes_back() {
        let (mut scheduler, root) = scheduler(2);
        let denied = child(root, scheduler.graph_mut());
        let flaky = child(root, scheduler.graph_mut());
        for (task, retryable) in [
            (denied, Retryability::UnknownOutcome),
            (flaky, Retryability::Retryable),
        ] {
            let admitted = scheduler.start(task, &request(), vec![]).expect("start");
            scheduler
                .finish(
                    admitted.attempt,
                    &AttemptOutcome::failed(Budget::default(), "provider error", 0)
                        .retryable(retryable),
                )
                .expect("finish");
        }
        // Backoff not elapsed yet.
        assert!(scheduler.retryable(0).is_empty());
        assert_eq!(scheduler.retryable(10_000), vec![flaky]);
        assert!(scheduler
            .retry(denied, &request(), vec![])
            .is_err_and(|error| matches!(
                error,
                SchedulerError::Graph(GraphError::NotRetryable(_, _))
            )));
    }

    #[test]
    fn a_retry_supersedes_the_attempt_it_replaces() {
        let (mut scheduler, root) = scheduler(2);
        let task = child(root, scheduler.graph_mut());
        let first = scheduler.start(task, &request(), vec![]).expect("start");
        scheduler
            .finish(
                first.attempt,
                &AttemptOutcome::failed(Budget::default(), "stream dropped", 0)
                    .retryable(Retryability::Retryable),
            )
            .expect("finish");
        let second = scheduler.retry(task, &request(), vec![]).expect("retry");
        assert_ne!(first.attempt, second.attempt);
        assert_eq!(
            scheduler.graph().attempt(first.attempt).map(|a| a.state),
            Some(AttemptState::Superseded)
        );
    }

    #[test]
    fn a_task_behind_a_failed_dependency_stops_being_pending_work() {
        let (mut scheduler, root) = scheduler(2);
        let upstream = child(root, scheduler.graph_mut());
        let downstream = TaskId::new();
        scheduler
            .graph_mut()
            .add(TaskNode {
                id: downstream,
                goal: "downstream".into(),
                dependencies: vec![upstream],
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
                runtime: TaskRuntime {
                    parent: Some(root),
                    ..TaskRuntime::default()
                },
            })
            .expect("downstream");
        let admitted = scheduler
            .start(upstream, &request(), vec![])
            .expect("start");
        scheduler
            .finish(
                admitted.attempt,
                &AttemptOutcome::failed(Budget::default(), "gave up", 0),
            )
            .expect("finish");
        assert_eq!(
            scheduler.graph().node(downstream).map(|node| node.state),
            Some(TaskState::Blocked)
        );
    }

    #[test]
    fn a_join_reads_the_states_its_policy_asks_about() {
        let (mut scheduler, root) = scheduler(4);
        let tasks: Vec<TaskId> = (0..3).map(|_| child(root, scheduler.graph_mut())).collect();
        for task in &tasks[..2] {
            let admitted = scheduler.start(*task, &request(), vec![]).expect("start");
            scheduler
                .finish(
                    admitted.attempt,
                    &AttemptOutcome::completed(Budget::default(), json!("done"), 1),
                )
                .expect("finish");
        }
        assert!(scheduler.joined(&tasks, JoinPolicy::Any));
        assert!(scheduler.joined(&tasks, JoinPolicy::Quorum(2)));
        assert!(!scheduler.joined(&tasks, JoinPolicy::All));
    }
}
