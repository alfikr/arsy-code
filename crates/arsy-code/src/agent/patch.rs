//! `fs.patch`: the `*** Begin Patch` dialect Codex established.
//!
//! The dialect is worth speaking rather than inventing one: models are trained
//! on it, and it carries context lines, so a hunk that no longer matches is
//! rejected instead of applied to the wrong place.
//!
//! Parsing is pure — [`parse`] turns text into [`Change`]s and [`update`]
//! rewrites a file's lines — and every path it produces is resolved through
//! [`Workspace`], so this module never decides for itself what is inside the
//! tree.

use crate::resource::{ResolveError, Workspace};
use arsy_kernel::{
    artifact::{ArtifactStore, NewArtifact, Sensitivity},
    capability::{CapabilityAction, CapabilityGrant},
    domain::ResourceRef,
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

const MAX_PATCHED_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// One file's worth of a patch.
#[derive(Debug, Eq, PartialEq)]
pub enum Change {
    Add {
        path: String,
        body: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        moved: Option<String>,
        hunks: Vec<Hunk>,
    },
}

impl Change {
    fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Delete { path } | Self::Update { path, .. } => path,
        }
    }
}

/// One contiguous edit: the lines to find, and what replaces them.
#[derive(Debug, Eq, PartialEq)]
pub struct Hunk {
    /// Context and removed lines, in file order — what must be there.
    pattern: Vec<String>,
    /// Context and added lines — what is left behind.
    replacement: Vec<String>,
    /// `*** End of File`: the pattern is anchored at the end of the file.
    at_end: bool,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PatchResult {
    pub changed: Vec<String>,
    pub summary: Vec<String>,
}

pub struct PatchExecutor {
    contract: OperationContract,
    workspace: PathBuf,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
}

impl PatchExecutor {
    pub fn new(
        workspace: &Workspace,
        artifacts: Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            contract: OperationContract {
                kind: OperationKind::new("fs.patch").expect("static operation kind is valid"),
                input_schema: InputSchema {
                    required: BTreeMap::from([("patch".to_owned(), JsonType::String)]),
                    optional: BTreeMap::new(),
                    allow_extra: false,
                },
                // A patch can create, rewrite, and remove in one call, so it
                // asks for delete authority as well as write: a policy that
                // allows edits but not deletions must be able to refuse it.
                actions: vec![CapabilityAction::FsWrite, CapabilityAction::FsDelete],
                idempotency: Idempotency::Effectful,
                // A patch may delete, and a deletion is not recoverable from
                // anything the harness keeps.
                reversible: false,
                concurrency: ConcurrencyRule::ExclusiveGlobal,
            },
            workspace: workspace.path().to_owned(),
            artifacts,
            retain_until_ms,
        })
    }
}

impl OperationExecutor for PatchExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let workspace = Workspace::open(&self.workspace)
            .map_err(|error| OperationError::Execution(error.to_string()))?;
        let patch = request
            .input
            .get("patch")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let result = apply(&workspace, patch)?;

        let bytes = serde_json::to_vec(&result)
            .map_err(|error| OperationError::Execution(error.to_string()))?;
        let value = self
            .artifacts
            .put(
                &bytes,
                NewArtifact {
                    media_type: "application/json".into(),
                    creator: request.actor.clone(),
                    source_revision: None,
                    sensitivity: Sensitivity::Internal,
                    retain_until_ms: self.retain_until_ms,
                },
            )
            .map(|metadata| metadata.resource_ref())
            .map_err(|error| OperationError::Execution(error.to_string()))?;

        let observed_effects = result
            .changed
            .iter()
            .filter_map(|path| {
                Some(Effect {
                    action: CapabilityAction::FsWrite,
                    resource: ResourceRef::new("workspace", path.clone()).ok()?,
                })
            })
            .collect();

        Ok(OperationOutcome {
            value: Some(value),
            observed_effects,
            evidence: Vec::new(),
            state: None,
        })
    }
}

/// Parse and apply a patch, reporting what each file change did.
///
/// The whole patch is parsed before anything is written, so a syntax error in
/// the last file does not leave the first one half-edited.
pub fn apply(workspace: &Workspace, patch: &str) -> Result<PatchResult, OperationError> {
    let changes = parse(patch).map_err(OperationError::Schema)?;
    let mut result = PatchResult {
        changed: Vec::new(),
        summary: Vec::new(),
    };
    for change in &changes {
        let path = change.path().to_owned();
        let described = match change {
            Change::Add { path, body } => {
                workspace
                    .create_new(path, body.as_bytes())
                    .map_err(|error| file_error(path, error))?;
                format!("added {path}")
            }
            Change::Delete { path } => {
                workspace
                    .remove(path)
                    .map_err(|error| file_error(path, error))?;
                format!("deleted {path}")
            }
            Change::Update { path, moved, hunks } => {
                let current = workspace
                    .read(path, MAX_PATCHED_FILE_BYTES)
                    .map_err(|error| file_error(path, error))?;
                let text = String::from_utf8(current.bytes)
                    .map_err(|_| OperationError::Schema(format!("{path} is not UTF-8 text")))?;
                let updated = update(&text, hunks)
                    .map_err(|error| OperationError::Schema(format!("{path}: {error}")))?;
                match moved {
                    Some(destination) => {
                        workspace
                            .write(destination, updated.as_bytes())
                            .map_err(|error| file_error(destination, error))?;
                        workspace
                            .remove(path)
                            .map_err(|error| file_error(path, error))?;
                        result.changed.push(destination.clone());
                        format!("updated {path} and moved it to {destination}")
                    }
                    None => {
                        workspace
                            .write(path, updated.as_bytes())
                            .map_err(|error| file_error(path, error))?;
                        format!("updated {path}")
                    }
                }
            }
        };
        result.changed.push(path);
        result.summary.push(described);
    }
    Ok(result)
}

fn file_error(path: &str, error: ResolveError) -> OperationError {
    match error {
        ResolveError::OutsideWorkspace | ResolveError::EmptyPath => {
            OperationError::Schema(format!("{path} is outside the workspace"))
        }
        ResolveError::AlreadyExists => {
            OperationError::Schema(format!("{path} already exists; use *** Update File"))
        }
        other => OperationError::Execution(format!("{path}: {other}")),
    }
}

pub fn parse(patch: &str) -> Result<Vec<Change>, String> {
    let mut lines = patch.lines().peekable();
    let begun = lines
        .next()
        .is_some_and(|line| line.trim() == "*** Begin Patch");
    if !begun {
        return Err("a patch starts with `*** Begin Patch`".to_owned());
    }
    let mut changes = Vec::new();
    while let Some(line) = lines.next() {
        if line.trim() == "*** End Patch" {
            return Ok(changes);
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let mut body = String::new();
            while lines.peek().is_some_and(|next| next.starts_with('+')) {
                body.push_str(&lines.next().expect("peeked")[1..]);
                body.push('\n');
            }
            changes.push(Change::Add {
                path: path.trim().to_owned(),
                body,
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            changes.push(Change::Delete {
                path: path.trim().to_owned(),
            });
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            let mut moved = None;
            if let Some(destination) = lines
                .peek()
                .and_then(|next| next.strip_prefix("*** Move to: "))
            {
                moved = Some(destination.trim().to_owned());
                lines.next();
            }
            let mut hunks: Vec<Hunk> = Vec::new();
            while let Some(next) = lines.peek() {
                if next.starts_with("*** ") && next.trim() != "*** End of File" {
                    break;
                }
                let next = lines.next().expect("peeked");
                if next.trim() == "*** End of File" {
                    if let Some(hunk) = hunks.last_mut() {
                        hunk.at_end = true;
                    }
                    continue;
                }
                // `@@` opens a hunk. Its text is a hint for a human reader; the
                // lines that follow are what has to match.
                if next.starts_with("@@") {
                    hunks.push(Hunk {
                        pattern: Vec::new(),
                        replacement: Vec::new(),
                        at_end: false,
                    });
                    continue;
                }
                let hunk = match hunks.last_mut() {
                    Some(hunk) => hunk,
                    None => {
                        hunks.push(Hunk {
                            pattern: Vec::new(),
                            replacement: Vec::new(),
                            at_end: false,
                        });
                        hunks.last_mut().expect("just pushed")
                    }
                };
                match next.chars().next() {
                    Some('+') => hunk.replacement.push(next[1..].to_owned()),
                    Some('-') => hunk.pattern.push(next[1..].to_owned()),
                    Some(' ') => {
                        hunk.pattern.push(next[1..].to_owned());
                        hunk.replacement.push(next[1..].to_owned());
                    }
                    // A blank line inside a hunk is an empty context line that
                    // lost its space in transit.
                    None => {
                        hunk.pattern.push(String::new());
                        hunk.replacement.push(String::new());
                    }
                    Some(_) => return Err(format!("`{next}` is not a patch line")),
                }
            }
            if hunks
                .iter()
                .all(|hunk| hunk.pattern.is_empty() && hunk.replacement.is_empty())
            {
                return Err(format!("the update of {} has no changes", path.trim()));
            }
            changes.push(Change::Update {
                path: path.trim().to_owned(),
                moved,
                hunks,
            });
        } else if !line.trim().is_empty() {
            return Err(format!("`{line}` is not a patch header"));
        }
    }
    Err("the patch is missing `*** End Patch`".to_owned())
}

fn update(current: &str, hunks: &[Hunk]) -> Result<String, String> {
    let trailing = current.ends_with('\n');
    let mut lines: Vec<String> = current.lines().map(str::to_owned).collect();
    let mut cursor = 0;
    for hunk in hunks {
        let at = seek(&lines, &hunk.pattern, cursor, hunk.at_end).ok_or_else(|| {
            // The nearest line the model would recognise, so a rejected hunk
            // says where to look rather than only that it failed.
            let wanted = hunk.pattern.first().map_or("", String::as_str);
            match nearest(&lines, wanted) {
                Some((line, text)) => format!(
                    "no line matched the context near `{wanted}`; the closest is line {line}: `{text}`"
                ),
                None => format!("no line matched the context near `{wanted}`"),
            }
        })?;
        lines.splice(at..at + hunk.pattern.len(), hunk.replacement.clone());
        // Later hunks apply after this one: a pattern is never matched inside
        // text an earlier hunk already replaced.
        cursor = at + hunk.replacement.len();
    }
    let mut out = lines.join("\n");
    if trailing && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// The line sharing the longest trimmed prefix with `wanted`.
///
/// Cheap and good enough for a diagnostic: the point is to show the model the
/// line it probably meant, not to rank the whole file.
fn nearest<'a>(lines: &'a [String], wanted: &str) -> Option<(usize, &'a str)> {
    let wanted = wanted.trim();
    if wanted.is_empty() {
        return None;
    }
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let shared = line
                .trim()
                .chars()
                .zip(wanted.chars())
                .take_while(|(left, right)| left == right)
                .count();
            (shared, index, line.as_str())
        })
        .filter(|(shared, _, _)| *shared > 0)
        .max_by_key(|(shared, _, _)| *shared)
        .map(|(_, index, line)| (index + 1, line))
}

/// Find `pattern` in `lines` at or after `start`, loosening on whitespace.
///
/// Exact first, then ignoring trailing whitespace, then ignoring both ends: a
/// model that reflows indentation should not have its edit rejected, but the
/// order means an exact match is never passed over for a looser one.
fn seek(lines: &[String], pattern: &[String], start: usize, at_end: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(if at_end { lines.len() } else { start });
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let comparisons: [fn(&str, &str) -> bool; 3] = [
        |a, b| a == b,
        |a, b| a.trim_end() == b.trim_end(),
        |a, b| a.trim() == b.trim(),
    ];
    let last = lines.len() - pattern.len();
    for same in comparisons {
        // An end-anchored hunk is tried at the tail first, which is what
        // `*** End of File` asks for.
        let candidates = at_end.then_some(last).into_iter().chain(start..=last);
        for at in candidates {
            if lines[at..at + pattern.len()]
                .iter()
                .zip(pattern)
                .all(|(line, want)| same(line, want))
            {
                return Some(at);
            }
        }
    }
    None
}
