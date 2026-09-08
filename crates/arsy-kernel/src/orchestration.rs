use crate::{
    capability::{AttenuationError, CapabilityAction, CapabilityGrant, ResourceScope},
    domain::{AgentId, CorrelationId, Principal, SessionId, TaskId},
    event::{EventEnvelope, EventPayload, EventStore, SchemaVersion, StoreError, StreamVersion},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Budget {
    pub tokens: u64,
    pub cost_micros: u64,
    pub wall_ms: u64,
}

impl Budget {
    pub const fn fits_within(self, parent: Self) -> bool {
        self.tokens <= parent.tokens
            && self.cost_micros <= parent.cost_micros
            && self.wall_ms <= parent.wall_ms
    }

    pub fn consume(&mut self, used: Self) -> Result<(), PartialEvidence> {
        if !used.fits_within(*self) {
            return Err(PartialEvidence {
                reason: "budget_exhausted".into(),
                remaining: *self,
            });
        }
        self.tokens -= used.tokens;
        self.cost_micros -= used.cost_micros;
        self.wall_ms -= used.wall_ms;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PartialEvidence {
    pub reason: String,
    pub remaining: Budget,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceRequirement {
    ReadOnlySnapshot,
    IsolatedWriter,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Ready,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskNode {
    pub id: TaskId,
    pub goal: String,
    pub dependencies: Vec<TaskId>,
    pub assignee: Option<AgentId>,
    pub required_output: String,
    pub workspace: WorkspaceRequirement,
    pub budget: Budget,
    pub authority: Vec<CapabilityGrant>,
    pub state: TaskState,
    pub lease_expires_at_ms: Option<u64>,
}

pub struct ChildCapabilityRequest {
    pub parent_grant: usize,
    pub action: CapabilityAction,
    pub scope: ResourceScope,
    pub expires_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinPolicy {
    All,
    Any,
    Quorum(usize),
}

impl JoinPolicy {
    pub fn satisfied(self, states: impl IntoIterator<Item = TaskState>) -> bool {
        let states: Vec<_> = states.into_iter().collect();
        let completed = states
            .iter()
            .filter(|state| **state == TaskState::Completed)
            .count();
        match self {
            Self::All => !states.is_empty() && completed == states.len(),
            Self::Any => completed > 0,
            Self::Quorum(required) => required > 0 && completed >= required,
        }
    }
}

pub struct TaskGraph {
    store: Arc<dyn EventStore>,
    session: SessionId,
    actor: Principal,
    version: StreamVersion,
    nodes: BTreeMap<TaskId, TaskNode>,
}

impl TaskGraph {
    pub fn new(
        store: Arc<dyn EventStore>,
        session: SessionId,
        actor: Principal,
    ) -> Result<Self, GraphError> {
        let version = store.current_version(session)?;
        let mut graph = Self {
            store,
            session,
            actor,
            version,
            nodes: BTreeMap::new(),
        };
        let limit = usize::try_from(version.0).unwrap_or(usize::MAX);
        for event in graph.store.read(session, 1, limit)? {
            graph.replay(&event)?;
        }
        Ok(graph)
    }

    pub fn add(&mut self, node: TaskNode) -> Result<(), GraphError> {
        if self.nodes.contains_key(&node.id) {
            return Err(GraphError::Duplicate(node.id));
        }
        if node.dependencies.contains(&node.id) || self.would_cycle(node.id, &node.dependencies) {
            self.record("task.cycle_detected", json!({"task_id": node.id}))?;
            return Err(GraphError::Cycle(node.id));
        }
        self.record("task.created", json!({"node": &node}))?;
        self.nodes.insert(node.id, node);
        Ok(())
    }

    pub fn add_child(
        &mut self,
        parent: TaskId,
        mut child: TaskNode,
        requests: Vec<ChildCapabilityRequest>,
    ) -> Result<(), GraphError> {
        let parent = self.nodes.get(&parent).ok_or(GraphError::Unknown(parent))?;
        if !child.budget.fits_within(parent.budget) {
            return Err(GraphError::BudgetExpansion);
        }
        let assignee = child.assignee.ok_or(GraphError::MissingAssignee)?;
        child.authority = requests
            .into_iter()
            .map(|request| {
                parent
                    .authority
                    .get(request.parent_grant)
                    .ok_or(GraphError::MissingParentGrant)?
                    .attenuate(
                        Principal::Agent(assignee),
                        request.action,
                        &request.scope,
                        request.expires_at_ms,
                    )
                    .map_err(GraphError::Attenuation)
            })
            .collect::<Result<_, _>>()?;
        self.add(child)
    }

    pub fn ready(&mut self) -> Result<Vec<TaskId>, GraphError> {
        let completed: BTreeSet<_> = self
            .nodes
            .iter()
            .filter_map(|(id, node)| (node.state == TaskState::Completed).then_some(*id))
            .collect();
        let ready: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(id, node)| {
                (node.state == TaskState::Pending
                    && node
                        .dependencies
                        .iter()
                        .all(|dependency| completed.contains(dependency)))
                .then_some(*id)
            })
            .collect();
        for id in &ready {
            self.transition(*id, TaskState::Ready, None)?;
        }
        Ok(ready)
    }

    pub fn lease(
        &mut self,
        id: TaskId,
        agent: AgentId,
        expires_at_ms: u64,
    ) -> Result<(), GraphError> {
        let node = self.nodes.get(&id).ok_or(GraphError::Unknown(id))?;
        if node.state != TaskState::Ready {
            return Err(GraphError::InvalidTransition(
                node.state,
                TaskState::Running,
            ));
        }
        self.record(
            "task.leased",
            json!({"task_id": id, "agent_id": agent, "expires_at_ms": expires_at_ms}),
        )?;
        let node = self.nodes.get_mut(&id).expect("checked above");
        node.assignee = Some(agent);
        node.lease_expires_at_ms = Some(expires_at_ms);
        node.state = TaskState::Running;
        Ok(())
    }

    pub fn complete(&mut self, id: TaskId, evidence: Value) -> Result<(), GraphError> {
        if self
            .nodes
            .get(&id)
            .is_some_and(|node| node.state == TaskState::Completed)
        {
            return Ok(());
        }
        self.transition(id, TaskState::Completed, Some(evidence))
    }

    /// Stop a task before it finished, with the reason.
    ///
    /// Allowed from any state, because cancelling is a decision about the
    /// future: a task that is pending never starts, and one that is running
    /// stops being anyone's to finish.
    pub fn cancel(&mut self, id: TaskId, reason: impl Into<String>) -> Result<(), GraphError> {
        let reason = reason.into();
        if self
            .nodes
            .get(&id)
            .is_some_and(|node| node.state == TaskState::Cancelled)
        {
            return Ok(());
        }
        self.transition(id, TaskState::Cancelled, Some(json!({"reason": reason})))
    }

    /// Record why a task stopped, keeping whatever it produced.
    ///
    /// Idempotent, like `complete`: a caller that fails a task twice — a
    /// retry, a resumed process closing what it found — is describing the same
    /// history, not writing a second one.
    pub fn fail(&mut self, id: TaskId, evidence: Value) -> Result<(), GraphError> {
        if self
            .nodes
            .get(&id)
            .is_some_and(|node| node.state == TaskState::Failed)
        {
            return Ok(());
        }
        self.transition(id, TaskState::Failed, Some(evidence))
    }

    /// Tasks waiting for someone to take them, in creation order.
    ///
    /// This is what makes a graph resumable: a process that died holding a
    /// lease leaves a task whose lease expires, and the next one finds it here
    /// with the goal it was created with.
    pub fn pending(&self) -> Vec<&TaskNode> {
        self.nodes
            .values()
            .filter(|node| matches!(node.state, TaskState::Ready | TaskState::Pending))
            .collect()
    }

    pub fn consume(&mut self, id: TaskId, used: Budget) -> Result<(), GraphError> {
        let mut remaining = self.nodes.get(&id).ok_or(GraphError::Unknown(id))?.budget;
        let result = remaining.consume(used);
        match result {
            Ok(()) => {
                self.record("task.budget_used", json!({"task_id": id, "used": used}))?;
                self.nodes.get_mut(&id).expect("checked above").budget = remaining;
                Ok(())
            }
            Err(evidence) => {
                self.record(
                    "task.budget_exhausted",
                    json!({"task_id": id, "partial_evidence": evidence}),
                )?;
                Err(GraphError::BudgetExhausted(evidence))
            }
        }
    }

    /// Hand every running task back to the queue, whatever its lease says.
    ///
    /// A lease expiring is how a *silent* holder is detected; this is for the
    /// case where the holder is known to be gone — its turn was found open by
    /// the process that came after it. Waiting out a lease we already know is
    /// dead would make resuming a killed run mean "come back in half an hour".
    pub fn reclaim_running(&mut self) -> Result<Vec<TaskId>, GraphError> {
        let running: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(id, node)| (node.state == TaskState::Running).then_some(*id))
            .collect();
        for id in &running {
            self.record("task.reclaimed", json!({"task_id": id}))?;
            let node = self.nodes.get_mut(id).expect("selected above");
            node.state = TaskState::Ready;
            node.assignee = None;
            node.lease_expires_at_ms = None;
        }
        Ok(running)
    }

    pub fn recover_expired(&mut self, now_ms: u64) -> Result<Vec<TaskId>, GraphError> {
        let expired: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(id, node)| {
                (node.state == TaskState::Running
                    && node
                        .lease_expires_at_ms
                        .is_some_and(|expiry| expiry <= now_ms))
                .then_some(*id)
            })
            .collect();
        for id in &expired {
            self.record("task.lease_expired", json!({"task_id": id}))?;
            let node = self.nodes.get_mut(id).expect("selected above");
            node.state = TaskState::Ready;
            node.assignee = None;
            node.lease_expires_at_ms = None;
        }
        Ok(expired)
    }

    pub fn node(&self, id: TaskId) -> Option<&TaskNode> {
        self.nodes.get(&id)
    }

    fn transition(
        &mut self,
        id: TaskId,
        target: TaskState,
        evidence: Option<Value>,
    ) -> Result<(), GraphError> {
        let current = self.nodes.get(&id).ok_or(GraphError::Unknown(id))?.state;
        let valid = matches!(
            (current, target),
            (TaskState::Pending, TaskState::Ready)
                | (TaskState::Ready, TaskState::Running)
                | (TaskState::Running, TaskState::Completed | TaskState::Failed)
                | (_, TaskState::Cancelled)
        );
        if !valid {
            return Err(GraphError::InvalidTransition(current, target));
        }
        self.record(
            "task.transitioned",
            json!({"task_id": id, "from": current, "to": target, "evidence": evidence}),
        )?;
        self.nodes.get_mut(&id).expect("checked above").state = target;
        Ok(())
    }

    fn would_cycle(&self, new: TaskId, dependencies: &[TaskId]) -> bool {
        let mut pending = dependencies.to_vec();
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if id == new {
                return true;
            }
            if seen.insert(id) {
                if let Some(node) = self.nodes.get(&id) {
                    pending.extend(&node.dependencies);
                }
            }
        }
        false
    }

    fn replay(&mut self, event: &EventEnvelope) -> Result<(), GraphError> {
        let EventPayload::Inline { data } = &event.payload else {
            return Ok(());
        };
        match event.kind.as_str() {
            "task.created" => {
                let node: TaskNode =
                    serde_json::from_value(data.get("node").cloned().ok_or_else(|| {
                        GraphError::InvalidEvent("task.created has no node".into())
                    })?)
                    .map_err(|error| GraphError::InvalidEvent(error.to_string()))?;
                self.nodes.insert(node.id, node);
            }
            "task.transitioned" => {
                let id = event_task_id(data)?;
                let state: TaskState =
                    serde_json::from_value(data.get("to").cloned().ok_or_else(|| {
                        GraphError::InvalidEvent("transition has no target".into())
                    })?)
                    .map_err(|error| GraphError::InvalidEvent(error.to_string()))?;
                self.nodes
                    .get_mut(&id)
                    .ok_or(GraphError::Unknown(id))?
                    .state = state;
            }
            "task.leased" => {
                let id = event_task_id(data)?;
                let node = self.nodes.get_mut(&id).ok_or(GraphError::Unknown(id))?;
                node.assignee =
                    Some(
                        serde_json::from_value(data.get("agent_id").cloned().ok_or_else(|| {
                            GraphError::InvalidEvent("lease has no agent".into())
                        })?)
                        .map_err(|error| GraphError::InvalidEvent(error.to_string()))?,
                    );
                node.lease_expires_at_ms = data.get("expires_at_ms").and_then(Value::as_u64);
                node.state = TaskState::Running;
            }
            "task.lease_expired" | "task.reclaimed" => {
                let id = event_task_id(data)?;
                let node = self.nodes.get_mut(&id).ok_or(GraphError::Unknown(id))?;
                node.state = TaskState::Ready;
                node.assignee = None;
                node.lease_expires_at_ms = None;
            }
            "task.budget_used" => {
                let id = event_task_id(data)?;
                let used: Budget =
                    serde_json::from_value(data.get("used").cloned().ok_or_else(|| {
                        GraphError::InvalidEvent("budget event has no usage".into())
                    })?)
                    .map_err(|error| GraphError::InvalidEvent(error.to_string()))?;
                self.nodes
                    .get_mut(&id)
                    .ok_or(GraphError::Unknown(id))?
                    .budget
                    .consume(used)
                    .map_err(GraphError::BudgetExhausted)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Append one graph event, catching up once if the stream moved.
    ///
    /// A task graph shares its session's stream with whatever else writes to
    /// it — the turn lifecycle, usage — because resuming a task means replaying
    /// one history, not correlating two. So a conflict here is the normal case
    /// of "something else appended since we last looked", not a lost update:
    /// the missed events are replayed into the graph and the append retried
    /// once. A second conflict is a genuinely contended stream and is reported.
    fn record(&mut self, kind: &str, payload: Value) -> Result<(), GraphError> {
        for attempt in 0..2 {
            let sequence = self.version.0.checked_add(1).ok_or(GraphError::Overflow)?;
            let event = EventEnvelope::new(
                self.session,
                sequence,
                self.actor.clone(),
                None,
                CorrelationId::new(),
                SchemaVersion(1),
                kind,
                EventPayload::Inline {
                    data: payload.clone(),
                },
            );
            match self.store.append(self.session, self.version, vec![event]) {
                Ok(version) => {
                    self.version = version;
                    return Ok(());
                }
                Err(StoreError::Conflict { .. }) if attempt == 0 => self.catch_up()?,
                Err(error) => return Err(error.into()),
            }
        }
        Err(GraphError::Store(StoreError::Conflict {
            expected: self.version,
            actual: self.store.current_version(self.session)?,
        }))
    }

    /// Replay everything appended to this session since the graph last looked.
    fn catch_up(&mut self) -> Result<(), GraphError> {
        loop {
            let page = self.store.read(
                self.session,
                self.version.0.checked_add(1).ok_or(GraphError::Overflow)?,
                MAX_CATCH_UP_BATCH,
            )?;
            let Some(last) = page.last() else {
                return Ok(());
            };
            let version = StreamVersion(last.sequence);
            for event in &page {
                self.replay(event)?;
            }
            self.version = version;
        }
    }
}

/// Events replayed per catch-up read. The same bound the service uses to page
/// a stream, for the same reason: a long session must not be read at once.
const MAX_CATCH_UP_BATCH: usize = 256;

pub fn attenuate_child_grant(
    parent: &CapabilityGrant,
    child: AgentId,
    action: CapabilityAction,
    scope: &ResourceScope,
    expiry: Option<u64>,
) -> Result<CapabilityGrant, AttenuationError> {
    parent.attenuate(Principal::Agent(child), action, scope, expiry)
}

#[derive(Debug)]
pub enum GraphError {
    Duplicate(TaskId),
    Unknown(TaskId),
    Cycle(TaskId),
    InvalidTransition(TaskState, TaskState),
    BudgetExhausted(PartialEvidence),
    BudgetExpansion,
    MissingAssignee,
    MissingParentGrant,
    Attenuation(AttenuationError),
    InvalidEvent(String),
    Overflow,
    Store(StoreError),
}

impl fmt::Display for GraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(id) => write!(formatter, "task {id} already exists"),
            Self::Unknown(id) => write!(formatter, "task {id} does not exist"),
            Self::Cycle(id) => write!(formatter, "task {id} introduces a dependency cycle"),
            Self::InvalidTransition(from, to) => {
                write!(formatter, "invalid task transition {from:?} -> {to:?}")
            }
            Self::BudgetExhausted(_) => {
                formatter.write_str("task budget exhausted with partial evidence")
            }
            Self::BudgetExpansion => formatter.write_str("child budget exceeds its parent budget"),
            Self::MissingAssignee => {
                formatter.write_str("delegated child task requires an assignee")
            }
            Self::MissingParentGrant => {
                formatter.write_str("child requested an unknown parent grant")
            }
            Self::Attenuation(error) => error.fmt(formatter),
            Self::InvalidEvent(message) => write!(formatter, "invalid task event: {message}"),
            Self::Overflow => formatter.write_str("task event sequence overflow"),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GraphError {}

impl From<StoreError> for GraphError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

fn event_task_id(data: &Value) -> Result<TaskId, GraphError> {
    serde_json::from_value(
        data.get("task_id")
            .cloned()
            .ok_or_else(|| GraphError::InvalidEvent("event has no task ID".into()))?,
    )
    .map_err(|error| GraphError::InvalidEvent(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MemoryEventStore;

    fn node(id: TaskId, dependencies: Vec<TaskId>, budget: Budget) -> TaskNode {
        TaskNode {
            id,
            goal: "bounded task".into(),
            dependencies,
            assignee: None,
            required_output: "evidence".into(),
            workspace: WorkspaceRequirement::ReadOnlySnapshot,
            budget,
            authority: Vec::new(),
            state: TaskState::Pending,
            lease_expires_at_ms: None,
        }
    }

    #[test]
    fn transitions_cycles_leases_and_budgets_are_durable_and_bounded() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut graph = TaskGraph::new(store.clone(), session, Principal::System).unwrap();
        let first = TaskId::new();
        graph
            .add(node(
                first,
                Vec::new(),
                Budget {
                    tokens: 10,
                    cost_micros: 20,
                    wall_ms: 30,
                },
            ))
            .unwrap();
        assert_eq!(graph.ready().unwrap(), vec![first]);
        graph.lease(first, AgentId::new(), 5).unwrap();
        assert_eq!(graph.recover_expired(5).unwrap(), vec![first]);
        graph.lease(first, AgentId::new(), 10).unwrap();
        assert!(matches!(
            graph.consume(
                first,
                Budget {
                    tokens: 11,
                    cost_micros: 0,
                    wall_ms: 0
                }
            ),
            Err(GraphError::BudgetExhausted(_))
        ));
        graph
            .complete(first, json!({"artifact": "partial"}))
            .unwrap();
        graph
            .complete(first, json!({"artifact": "duplicate"}))
            .unwrap();

        let cycle = TaskId::new();
        assert!(matches!(
            graph.add(node(cycle, vec![cycle], Budget::default())),
            Err(GraphError::Cycle(id)) if id == cycle
        ));
        let events = store.read(session, 1, 32).unwrap();
        assert!(events
            .iter()
            .any(|event| event.kind == "task.cycle_detected"));
        assert!(events
            .iter()
            .any(|event| event.kind == "task.budget_exhausted"));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "task.transitioned")
                .count(),
            2,
            "idempotent completion appends once"
        );
        let rebuilt = TaskGraph::new(store, session, Principal::System).unwrap();
        assert_eq!(rebuilt.node(first).unwrap().state, TaskState::Completed);
        assert_eq!(rebuilt.node(first).unwrap().budget.tokens, 10);
    }

    #[test]
    fn child_budget_and_authority_are_derived_from_the_parent() {
        use crate::{
            capability::{PolicySource, ResourcePattern},
            domain::GrantId,
        };
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let mut graph = TaskGraph::new(store, SessionId::new(), Principal::System).unwrap();
        let parent_id = TaskId::new();
        let scope = ResourceScope::single(ResourcePattern::new("file", "/repo/**").unwrap());
        let mut parent = node(
            parent_id,
            Vec::new(),
            Budget {
                tokens: 10,
                cost_micros: 10,
                wall_ms: 10,
            },
        );
        parent.authority.push(CapabilityGrant {
            id: GrantId::new(),
            actor: Principal::System,
            action: CapabilityAction::FsRead,
            scope: scope.clone(),
            expires_at_ms: None,
            delegation_depth: 1,
            source: PolicySource::User,
        });
        graph.add(parent).unwrap();
        let mut child = node(
            TaskId::new(),
            vec![parent_id],
            Budget {
                tokens: 5,
                cost_micros: 5,
                wall_ms: 5,
            },
        );
        child.assignee = Some(AgentId::new());
        graph
            .add_child(
                parent_id,
                child,
                vec![ChildCapabilityRequest {
                    parent_grant: 0,
                    action: CapabilityAction::FsRead,
                    scope,
                    expires_at_ms: None,
                }],
            )
            .unwrap();
    }
}
