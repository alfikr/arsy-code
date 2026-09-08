//! The workspace file operations, as contracts the registry can dispatch.
//!
//! One executor covers all of them because they differ only in which
//! [`Workspace`] call they make and which capability that call needs. Splitting
//! them into seven types would duplicate the artifact write, the resource
//! naming, and the error mapping seven times over for no case that varies.
//!
//! Every one of them goes through [`Workspace`], so confinement is decided in
//! one place; none of them touch `std::fs` directly.

use crate::{
    edit::{self, EditAddress, EditOperation},
    resource::{DirEntry, ResolveError, Workspace},
};
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
use serde_json::{json, Value};
use std::{num::NonZeroU32, path::PathBuf, sync::Arc};

/// The most one call will read or write. Larger than a source file and smaller
/// than anything a turn could carry, so the bound is hit by a mistake rather
/// than by ordinary work.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// What a file operation does. The kind string is the operation's identity, so
/// it is defined here rather than at each construction site.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileOperation {
    Read,
    List,
    Write,
    Create,
    Edit,
    Delete,
    Move,
}

impl FileOperation {
    pub const ALL: [Self; 7] = [
        Self::Read,
        Self::List,
        Self::Write,
        Self::Create,
        Self::Edit,
        Self::Delete,
        Self::Move,
    ];

    const fn kind(self) -> &'static str {
        match self {
            Self::Read => "fs.read",
            Self::List => "fs.list",
            Self::Write => "fs.write",
            Self::Create => "fs.create",
            Self::Edit => "fs.edit",
            Self::Delete => "fs.delete",
            Self::Move => "fs.move",
        }
    }

    const fn action(self) -> CapabilityAction {
        match self {
            Self::Read | Self::List => CapabilityAction::FsRead,
            Self::Write | Self::Create | Self::Edit | Self::Move => CapabilityAction::FsWrite,
            Self::Delete => CapabilityAction::FsDelete,
        }
    }

    /// Reading twice is the same answer; editing twice is not the same file.
    const fn idempotency(self) -> Idempotency {
        match self {
            Self::Read | Self::List | Self::Write => Idempotency::Idempotent,
            Self::Create | Self::Edit | Self::Delete | Self::Move => Idempotency::Effectful,
        }
    }

    /// Whether the effect can be undone.
    ///
    /// Rewriting or moving a file leaves the previous content in version
    /// control; removing one does not, and neither does anything the harness
    /// keeps. Policy raises an irreversible call to approval however permissive
    /// a rule is, so this is the line between "an operator who allowed edits
    /// gets edits" and "an operator is asked about every line the model
    /// writes".
    const fn reversible(self) -> bool {
        !matches!(self, Self::Delete)
    }

    fn schema(self) -> InputSchema {
        let string = |name: &str| (name.to_owned(), JsonType::String);
        let number = |name: &str| (name.to_owned(), JsonType::Number);
        let (required, optional) = match self {
            Self::Read => (
                vec![string("path")],
                vec![number("offset"), number("limit")],
            ),
            Self::List => (Vec::new(), vec![string("path")]),
            Self::Write | Self::Create => (vec![string("path"), string("content")], Vec::new()),
            Self::Edit => (
                vec![string("path"), string("old_text"), string("new_text")],
                vec![number("occurrence")],
            ),
            Self::Delete => (vec![string("path")], Vec::new()),
            Self::Move => (vec![string("from"), string("to")], Vec::new()),
        };
        InputSchema {
            required: required.into_iter().collect(),
            optional: optional.into_iter().collect(),
            allow_extra: false,
        }
    }
}

/// The bytes a read returned, plus what the model needs in order to ask a
/// better second question: whether it saw the whole file, and where it stopped.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReadResult {
    pub path: String,
    pub text: String,
    /// One-based line the excerpt starts at, so a later edit can be addressed.
    pub first_line: u64,
    pub lines_returned: u64,
    pub total_lines: u64,
    pub truncated: bool,
    pub binary: bool,
    pub digest: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ListResult {
    pub path: String,
    pub entries: Vec<ListEntry>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ListEntry {
    pub name: String,
    pub directory: bool,
    pub bytes: u64,
}

/// What a mutation did. `digest` is the file's state afterwards, which is what
/// a caller checks before editing the same file again.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WriteResult {
    pub path: String,
    pub created: bool,
    pub bytes: u64,
    pub digest: String,
}

pub struct FileExecutor {
    operation: FileOperation,
    contract: OperationContract,
    workspace: PathBuf,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
}

impl FileExecutor {
    pub fn new(
        operation: FileOperation,
        workspace: &Workspace,
        artifacts: Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            operation,
            contract: OperationContract {
                kind: OperationKind::new(operation.kind()).expect("static operation kind is valid"),
                input_schema: operation.schema(),
                actions: vec![operation.action()],
                idempotency: operation.idempotency(),
                reversible: operation.reversible(),
                concurrency: match operation {
                    FileOperation::Read | FileOperation::List => ConcurrencyRule::Parallel,
                    _ => ConcurrencyRule::ExclusivePerResource,
                },
            },
            workspace: workspace.path().to_owned(),
            artifacts,
            retain_until_ms,
        })
    }

    fn open(&self) -> Result<Workspace, OperationError> {
        Workspace::open(&self.workspace)
            .map_err(|error| OperationError::Execution(error.to_string()))
    }

    fn put(
        &self,
        value: &impl Serialize,
        creator: Principal,
    ) -> Result<ResourceRef, OperationError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| OperationError::Execution(error.to_string()))?;
        self.artifacts
            .put(
                &bytes,
                NewArtifact {
                    media_type: "application/json".into(),
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

impl OperationExecutor for FileExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let workspace = self.open()?;
        let input = &request.input;
        let string = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or_default();
        let number = |key: &str| input.get(key).and_then(Value::as_u64);

        let (value, touched, state) = match self.operation {
            FileOperation::Read => {
                let path = string("path");
                let result = read(&workspace, path, number("offset"), number("limit"))?;
                let digest = result.digest.clone();
                (
                    self.put(&result, request.actor.clone())?,
                    path.to_owned(),
                    Some(digest),
                )
            }
            FileOperation::List => {
                let path = input.get("path").and_then(Value::as_str).unwrap_or(".");
                let entries = workspace.list(path).map_err(resolve)?;
                (
                    self.put(
                        &ListResult {
                            path: path.to_owned(),
                            entries: entries.into_iter().map(entry).collect(),
                        },
                        request.actor.clone(),
                    )?,
                    path.to_owned(),
                    None,
                )
            }
            FileOperation::Write | FileOperation::Create => {
                let path = string("path");
                let content = string("content");
                let existed = workspace.read(path, MAX_FILE_BYTES).is_ok();
                let digest = if self.operation == FileOperation::Create {
                    workspace.create_new(path, content.as_bytes())
                } else {
                    workspace.write(path, content.as_bytes())
                }
                .map_err(resolve)?;
                (
                    self.put(
                        &WriteResult {
                            path: path.to_owned(),
                            created: !existed,
                            bytes: content.len() as u64,
                            digest: digest.to_string(),
                        },
                        request.actor.clone(),
                    )?,
                    path.to_owned(),
                    Some(digest.to_string()),
                )
            }
            FileOperation::Edit => {
                let path = string("path");
                let result = apply_edit(
                    &workspace,
                    path,
                    string("old_text"),
                    string("new_text"),
                    number("occurrence"),
                )?;
                let digest = result.digest.clone();
                (
                    self.put(&result, request.actor.clone())?,
                    path.to_owned(),
                    Some(digest),
                )
            }
            FileOperation::Delete => {
                let path = string("path");
                workspace.remove(path).map_err(resolve)?;
                (
                    self.put(
                        &json!({"path": path, "deleted": true}),
                        request.actor.clone(),
                    )?,
                    path.to_owned(),
                    None,
                )
            }
            FileOperation::Move => {
                let from = string("from");
                let to = string("to");
                workspace.rename(from, to).map_err(resolve)?;
                (
                    self.put(&json!({"from": from, "to": to}), request.actor.clone())?,
                    to.to_owned(),
                    None,
                )
            }
        };

        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: self.operation.action(),
                resource: ResourceRef::new("workspace", touched)
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
            }],
            evidence: Vec::new(),
            state: state.and_then(|digest| digest.parse().ok()),
        })
    }
}

fn entry(entry: DirEntry) -> ListEntry {
    ListEntry {
        name: entry.name,
        directory: entry.directory,
        bytes: entry.bytes,
    }
}

/// Read a file, optionally a window of it.
///
/// The window is in lines rather than bytes because that is the unit a model
/// asks in and the unit an error message reports in; a byte offset would let a
/// second read start mid-character.
fn read(
    workspace: &Workspace,
    path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
) -> Result<ReadResult, OperationError> {
    let content = workspace.read(path, MAX_FILE_BYTES).map_err(resolve)?;
    let digest = content.digest.to_string();
    if content.is_binary() {
        // A binary file is reported rather than decoded: lossy UTF-8 would fill
        // a turn with replacement characters and teach the model nothing.
        return Ok(ReadResult {
            path: path.to_owned(),
            text: String::new(),
            first_line: 0,
            lines_returned: 0,
            total_lines: 0,
            truncated: false,
            binary: true,
            digest,
        });
    }
    let text = String::from_utf8(content.bytes)
        .map_err(|error| OperationError::Execution(error.to_string()))?;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len() as u64;
    let start = offset.unwrap_or(1).max(1) - 1;
    let count = limit.unwrap_or(u64::MAX);
    let window: Vec<&str> = lines
        .iter()
        .skip(usize::try_from(start).unwrap_or(usize::MAX))
        .take(usize::try_from(count).unwrap_or(usize::MAX))
        .copied()
        .collect();
    let returned = window.len() as u64;
    Ok(ReadResult {
        path: path.to_owned(),
        text: window.join("\n"),
        first_line: start + 1,
        lines_returned: returned,
        total_lines: total,
        truncated: start + returned < total,
        binary: false,
        digest,
    })
}

/// Replace one occurrence of `old_text`, refusing an ambiguous one.
///
/// Delegates to the edit engine so an agent edit and a transactional edit share
/// one matcher: an anchor that appears twice is [`edit::EditError::Ambiguous`]
/// here for the same reason it is there, rather than silently taking the first.
fn apply_edit(
    workspace: &Workspace,
    path: &str,
    old_text: &str,
    new_text: &str,
    occurrence: Option<u64>,
) -> Result<WriteResult, OperationError> {
    if old_text.is_empty() {
        return Err(OperationError::Schema(
            "old_text must not be empty; use fs.write to replace a whole file".into(),
        ));
    }
    let occurrence = match occurrence {
        Some(value) => Some(
            u32::try_from(value)
                .ok()
                .and_then(NonZeroU32::new)
                .ok_or_else(|| OperationError::Schema("occurrence is one-based".into()))?,
        ),
        None => None,
    };
    let edits = edit::apply_unversioned(
        workspace.path(),
        &[EditOperation {
            path: PathBuf::from(path),
            address: EditAddress::TextAnchor {
                needle: old_text.to_owned(),
                occurrence,
            },
            replacement: new_text.as_bytes().to_vec(),
        }],
    )
    .map_err(|error| OperationError::Execution(error.to_string()))?;
    let applied = edits
        .first()
        .ok_or_else(|| OperationError::Execution("edit applied nothing".into()))?;
    Ok(WriteResult {
        path: path.to_owned(),
        created: false,
        bytes: workspace
            .read(path, MAX_FILE_BYTES)
            .map(|content| content.bytes.len() as u64)
            .unwrap_or_default(),
        digest: applied.after.to_string(),
    })
}

/// A resolve failure the model can act on.
///
/// `OutsideWorkspace` and `AlreadyExists` are decisions, not faults, so they
/// keep their own wording; everything else is an I/O condition reported as it
/// happened.
fn resolve(error: ResolveError) -> OperationError {
    match error {
        ResolveError::OutsideWorkspace | ResolveError::EmptyPath | ResolveError::AlreadyExists => {
            OperationError::Schema(error.to_string())
        }
        other => OperationError::Execution(other.to_string()),
    }
}

/// The registry entries this module contributes.
pub fn executors(
    workspace: &Workspace,
    artifacts: &Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
) -> Vec<Arc<dyn OperationExecutor>> {
    FileOperation::ALL
        .into_iter()
        .map(|operation| {
            FileExecutor::new(operation, workspace, Arc::clone(artifacts), retain_until_ms)
                as Arc<dyn OperationExecutor>
        })
        .collect()
}
