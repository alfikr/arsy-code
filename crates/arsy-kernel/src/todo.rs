//! A durable checklist for one session: what still has to happen, in order.
//!
//! # Why this is not the plan, and not the task graph
//!
//! Three things in ARSY look like lists of work and are not the same list:
//!
//! - the **plan** (`plan.*`) is the model's scratch pad for the turn it is in.
//!   It lives in memory, it is rewritten constantly, and losing it costs
//!   nothing once the turn is over.
//! - the **task graph** ([`orchestration`](crate::orchestration)) is the
//!   scheduler: budgets, leases, delegated authority, which agent holds what.
//!   Its states are about execution, not intent.
//! - a **TODO** is the commitment. It survives the turn, it survives the
//!   process, and it is the thing a person reads to find out what is left.
//!
//! Collapsing any two of them would make one of the three worse: a plan that
//! had to be durable could not be rewritten freely, and a checklist stored in
//! the scheduler would inherit a lease and a budget it has no use for.
//!
//! # Durability
//!
//! Every change is an event on the session's own stream, so the list is
//! rebuilt by replaying it. That is what makes a resumed session show the same
//! checklist the interrupted one had, and why the history of a TODO — created,
//! started, finished, dropped — is auditable rather than merely current.

use crate::{
    domain::{CorrelationId, Principal, SessionId},
    event::{EventEnvelope, EventPayload, EventStore, SchemaVersion, StoreError, StreamVersion},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fmt, sync::Arc};

/// Events replayed per catch-up read, matching the rest of the kernel.
const MAX_CATCH_UP_BATCH: usize = 256;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    /// Abandoned on purpose. Kept rather than deleted, so a checklist that
    /// shrank can still say what was dropped and when.
    Cancelled,
}

impl TodoStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Whether nothing more is expected to happen to an item in this state.
    pub const fn is_settled(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TodoItem {
    pub id: String,
    pub text: String,
    pub status: TodoStatus,
    /// Ids this item waits for. Kept whatever their state, so the reason an
    /// item is blocked survives the item that blocked it being completed.
    pub depends_on: Vec<String>,
    /// Who asked for it: the operator, or the model working the task.
    pub author: TodoAuthor,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoAuthor {
    User,
    Model,
}

impl TodoItem {
    /// The ids this item is still waiting on, given the rest of the list.
    ///
    /// A dependency that was cancelled does not block: dropping a step is a
    /// decision that it will not happen, and leaving its dependants stuck
    /// would make the checklist unusable rather than accurate.
    pub fn blocked_by<'a>(&'a self, items: &'a [Self]) -> Vec<&'a str> {
        self.depends_on
            .iter()
            .filter(|id| {
                items
                    .iter()
                    .find(|item| item.id == **id)
                    .is_none_or(|item| !item.status.is_settled())
            })
            .map(String::as_str)
            .collect()
    }
}

/// The list, plus the counts a caller would otherwise compute itself.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TodoSnapshot {
    pub items: Vec<TodoItem>,
    pub total: usize,
    pub completed: usize,
    /// The first unsettled item, which is what "where are we" means.
    pub current: Option<String>,
}

/// One session's checklist, rebuilt from its event stream.
pub struct TodoList {
    store: Arc<dyn EventStore>,
    session: SessionId,
    actor: Principal,
    version: StreamVersion,
    items: Vec<TodoItem>,
    next_id: u64,
}

impl TodoList {
    /// Read the session's stream and rebuild the checklist it left.
    pub fn open(
        store: Arc<dyn EventStore>,
        session: SessionId,
        actor: Principal,
    ) -> Result<Self, TodoError> {
        let mut list = Self {
            version: StreamVersion(0),
            store,
            session,
            actor,
            items: Vec::new(),
            next_id: 0,
        };
        list.catch_up()?;
        Ok(list)
    }

    pub fn snapshot(&self) -> TodoSnapshot {
        TodoSnapshot {
            total: self.items.len(),
            completed: self
                .items
                .iter()
                .filter(|item| item.status == TodoStatus::Completed)
                .count(),
            current: self
                .items
                .iter()
                .find(|item| !item.status.is_settled())
                .map(|item| item.id.clone()),
            items: self.items.clone(),
        }
    }

    /// Append an item to the end of the checklist.
    pub fn add(
        &mut self,
        text: &str,
        depends_on: Vec<String>,
        author: TodoAuthor,
    ) -> Result<TodoItem, TodoError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(TodoError::Empty);
        }
        for id in &depends_on {
            if !self.items.iter().any(|item| item.id == *id) {
                return Err(TodoError::Unknown(id.clone()));
            }
        }
        self.next_id += 1;
        let item = TodoItem {
            id: format!("todo-{}", self.next_id),
            text: text.to_owned(),
            status: TodoStatus::Pending,
            depends_on,
            author,
        };
        self.record("todo.created", json!({"item": &item}))?;
        self.items.push(item.clone());
        Ok(item)
    }

    /// Change an item's text, its status, or both.
    ///
    /// Starting an item whose dependencies are unsettled is refused rather
    /// than allowed and flagged: the point of recording a dependency is that
    /// something declines to proceed without it.
    pub fn update(
        &mut self,
        id: &str,
        status: Option<TodoStatus>,
        text: Option<&str>,
    ) -> Result<TodoItem, TodoError> {
        let position = self
            .items
            .iter()
            .position(|item| item.id == id)
            .ok_or_else(|| TodoError::Unknown(id.to_owned()))?;
        if matches!(status, Some(TodoStatus::InProgress | TodoStatus::Completed)) {
            let blocked: Vec<String> = self.items[position]
                .blocked_by(&self.items)
                .into_iter()
                .map(str::to_owned)
                .collect();
            if !blocked.is_empty() {
                return Err(TodoError::Blocked(id.to_owned(), blocked));
            }
        }
        let mut updated = self.items[position].clone();
        if let Some(status) = status {
            updated.status = status;
        }
        if let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) {
            updated.text = text.to_owned();
        }
        self.record("todo.updated", json!({"item": &updated}))?;
        self.items[position] = updated.clone();
        Ok(updated)
    }

    /// Drop an item without losing that it existed.
    pub fn cancel(&mut self, id: &str) -> Result<TodoItem, TodoError> {
        self.update(id, Some(TodoStatus::Cancelled), None)
    }

    /// Put the checklist in a new order, which must name every item once.
    pub fn reorder(&mut self, order: &[String]) -> Result<(), TodoError> {
        let mut current: Vec<&str> = self.items.iter().map(|item| item.id.as_str()).collect();
        let mut requested: Vec<&str> = order.iter().map(String::as_str).collect();
        current.sort_unstable();
        requested.sort_unstable();
        if current != requested {
            return Err(TodoError::Order);
        }
        let reordered: Vec<TodoItem> = order
            .iter()
            .map(|id| {
                self.items
                    .iter()
                    .find(|item| item.id == *id)
                    .cloned()
                    .expect("checked above")
            })
            .collect();
        self.record("todo.reordered", json!({"order": order}))?;
        self.items = reordered;
        Ok(())
    }

    /// Append one event, replaying and retrying once if the stream moved.
    ///
    /// The checklist shares its session's stream with the turn lifecycle, so a
    /// conflict is the ordinary case of something else having appended since
    /// the last look rather than a lost update.
    fn record(&mut self, kind: &str, payload: Value) -> Result<(), TodoError> {
        for attempt in 0..2 {
            let sequence = self.version.0.checked_add(1).ok_or(TodoError::Overflow)?;
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
                Err(error) => return Err(TodoError::Store(error)),
            }
        }
        Err(TodoError::Contended)
    }

    fn catch_up(&mut self) -> Result<(), TodoError> {
        loop {
            let from = self.version.0.checked_add(1).ok_or(TodoError::Overflow)?;
            let page = self
                .store
                .read(self.session, from, MAX_CATCH_UP_BATCH)
                .map_err(TodoError::Store)?;
            let Some(last) = page.last() else {
                return Ok(());
            };
            let version = StreamVersion(last.sequence);
            for event in &page {
                self.replay(event);
            }
            self.version = version;
        }
    }

    /// Fold one event into the list.
    ///
    /// A malformed `todo.*` event is skipped rather than fatal: the checklist
    /// is a convenience over a shared stream, and refusing to open a session
    /// because one entry cannot be read would be a worse failure than showing
    /// the rest of it.
    fn replay(&mut self, event: &EventEnvelope) {
        let EventPayload::Inline { data } = &event.payload else {
            return;
        };
        match event.kind.as_str() {
            "todo.created" | "todo.updated" => {
                let Some(item) = data
                    .get("item")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<TodoItem>(value).ok())
                else {
                    return;
                };
                self.next_id = self.next_id.max(numeric_suffix(&item.id));
                match self.items.iter().position(|held| held.id == item.id) {
                    Some(position) => self.items[position] = item,
                    None => self.items.push(item),
                }
            }
            "todo.reordered" => {
                let order: Vec<String> = data
                    .get("order")
                    .and_then(Value::as_array)
                    .map(|order| {
                        order
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                self.items.sort_by_key(|item| {
                    order
                        .iter()
                        .position(|id| *id == item.id)
                        .unwrap_or(usize::MAX)
                });
            }
            _ => {}
        }
    }
}

/// The number in `todo-7`, so replay continues the numbering rather than
/// restarting it and colliding with an item the stream already holds.
fn numeric_suffix(id: &str) -> u64 {
    id.rsplit('-')
        .next()
        .and_then(|tail| tail.parse().ok())
        .unwrap_or(0)
}

#[derive(Debug)]
pub enum TodoError {
    Empty,
    Unknown(String),
    Blocked(String, Vec<String>),
    Order,
    Overflow,
    Contended,
    Store(StoreError),
}

impl fmt::Display for TodoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("a TODO needs something to say"),
            Self::Unknown(id) => write!(formatter, "no TODO is called `{id}`"),
            Self::Blocked(id, blocked) => write!(
                formatter,
                "`{id}` waits for {}; finish or cancel those first",
                blocked.join(", ")
            ),
            Self::Order => {
                formatter.write_str("`order` must name exactly the current TODOs, once each")
            }
            Self::Overflow => formatter.write_str("session event sequence overflow"),
            Self::Contended => formatter.write_str("the session stream is being written elsewhere"),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TodoError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MemoryEventStore;

    fn list(store: &Arc<dyn EventStore>, session: SessionId) -> TodoList {
        TodoList::open(Arc::clone(store), session, Principal::System).unwrap()
    }

    #[test]
    fn a_checklist_survives_being_rebuilt_from_the_stream_it_was_written_to() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();

        let mut todos = list(&store, session);
        let first = todos
            .add("read the failing test", Vec::new(), TodoAuthor::User)
            .unwrap();
        let second = todos
            .add("fix it", vec![first.id.clone()], TodoAuthor::Model)
            .unwrap();
        todos
            .update(&first.id, Some(TodoStatus::InProgress), None)
            .unwrap();

        // A dependant cannot start while what it waits for is unfinished.
        let blocked = todos
            .update(&second.id, Some(TodoStatus::InProgress), None)
            .unwrap_err();
        assert!(
            blocked.to_string().contains(&first.id),
            "the refusal names what is in the way: {blocked}"
        );

        todos
            .update(&first.id, Some(TodoStatus::Completed), None)
            .unwrap();
        todos
            .update(
                &second.id,
                Some(TodoStatus::InProgress),
                Some("fix the bug"),
            )
            .unwrap();

        // A second process opening the same session sees the same checklist.
        let resumed = list(&store, session);
        let snapshot = resumed.snapshot();
        assert_eq!(snapshot.total, 2);
        assert_eq!(snapshot.completed, 1);
        assert_eq!(snapshot.current.as_deref(), Some(second.id.as_str()));
        assert_eq!(snapshot.items[0].status, TodoStatus::Completed);
        assert_eq!(snapshot.items[1].text, "fix the bug");
        assert_eq!(snapshot.items[1].author, TodoAuthor::Model);
        assert_eq!(snapshot.items[1].depends_on, vec![first.id.clone()]);
    }

    #[test]
    fn a_cancelled_dependency_stops_blocking_and_stays_on_the_record() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut todos = list(&store, session);

        let first = todos
            .add("ask the operator", Vec::new(), TodoAuthor::Model)
            .unwrap();
        let second = todos
            .add(
                "act on the answer",
                vec![first.id.clone()],
                TodoAuthor::Model,
            )
            .unwrap();
        todos.cancel(&first.id).unwrap();
        todos
            .update(&second.id, Some(TodoStatus::Completed), None)
            .unwrap();

        let snapshot = list(&store, session).snapshot();
        assert_eq!(snapshot.items[0].status, TodoStatus::Cancelled);
        assert_eq!(snapshot.total, 2, "a cancelled item is kept, not deleted");
        assert_eq!(snapshot.current, None, "nothing is left to do");
    }

    #[test]
    fn reordering_must_name_every_item_and_is_replayed_in_the_new_order() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let mut todos = list(&store, session);
        let first = todos
            .add("second, actually", Vec::new(), TodoAuthor::User)
            .unwrap();
        let second = todos
            .add("first, actually", Vec::new(), TodoAuthor::User)
            .unwrap();

        assert!(todos.reorder(&[second.id.clone()]).is_err());
        todos
            .reorder(&[second.id.clone(), first.id.clone()])
            .unwrap();

        let snapshot = list(&store, session).snapshot();
        assert_eq!(snapshot.items[0].id, second.id);
        assert_eq!(snapshot.items[1].id, first.id);
    }

    /// Ids continue where the stream left off, so a resumed session cannot
    /// create a second `todo-1` that overwrites the first on the next replay.
    #[test]
    fn ids_continue_across_a_resume() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());
        let session = SessionId::new();
        let first = list(&store, session)
            .add("one", Vec::new(), TodoAuthor::User)
            .unwrap();

        let second = list(&store, session)
            .add("two", Vec::new(), TodoAuthor::User)
            .unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(list(&store, session).snapshot().total, 2);
    }
}
