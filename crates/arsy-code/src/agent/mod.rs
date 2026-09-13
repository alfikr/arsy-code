//! The harness core: what the model may call, and what happens when it does.
//!
//! # Why this layer exists
//!
//! There is exactly one path from a model tool call to an effect:
//!
//! ```text
//! model tool call
//!       │  ToolRuntime::decode   — model-facing name and arguments
//!       ▼
//! OperationRequest              — kind, actor, named requirements
//!       │  RuleSet::evaluate    — allow / require approval / deny
//!       ▼
//! CapabilityGrant[]
//!       │  OperationRegistry::dispatch — schema check, grant check, execute
//!       ▼
//! OperationOutcome              — result and evidence, as artifacts
//!       │  ToolRuntime::render  — bounded text the model can read
//!       ▼
//! ToolResult
//! ```
//!
//! Nothing bypasses it. The MCP server, the TUI, and `arsy run` all decode into
//! the same `OperationRequest`, so a capability decision is made once rather
//! than once per front end, and a tool that runs leaves the same audit trail
//! whichever surface asked for it.
//!
//! # Why the model-facing names differ from the operation kinds
//!
//! `process.exec` takes `argv`, `timeout_ms`, and `max_output_bytes`, all
//! required — the right contract for an audit record and a poor one for a
//! model. [`Tool`] is that adaptation and only that: a name, a description, a
//! JSON Schema, and the function that turns the model's arguments into the
//! operation's. It holds no policy, no state, and no I/O.

pub mod budget;
pub mod codeops;
#[cfg(feature = "dap")]
pub mod debugops;
pub mod discoveryops;
pub mod fsops;
pub mod instructions;
pub mod patch;
pub mod planops;
#[cfg(feature = "wasm")]
pub mod pluginops;
pub mod searchops;
pub mod validateops;

use crate::resource::Workspace;
use arsy_kernel::{
    artifact::{unix_time_ms, ArtifactReadLimits, ArtifactStore},
    capability::{CapabilityAction, CapabilityGrant, CapabilityRequirement},
    domain::{ArtifactId, OperationId, Principal},
    operation::{
        OperationError, OperationKind, OperationOutcome, OperationRegistry, OperationRequest,
    },
    policy::{ApprovalRequest, PolicyDecision, PolicyQuery, RiskContext, RuleSet},
    provider::ToolSchema,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

/// Enough output to read a test failure, little enough to leave a turn's
/// context for the answer.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 16 * 1024;

/// Store an operation's result and name it.
///
/// Every executor here returns its result as an artifact rather than inline,
/// which is what makes a tool call replayable and what lets the transcript be
/// trimmed without losing anything. That is one decision, so it is written
/// once: `fs.*`, `search.*`, and `fs.patch` would otherwise each carry their
/// own copy of the media type, the sensitivity, and the error mapping, and a
/// change to any of them would have to be made three times.
pub(crate) fn store(
    artifacts: &dyn ArtifactStore,
    value: &impl serde::Serialize,
    creator: Principal,
    retain_until_ms: u64,
) -> Result<arsy_kernel::domain::ResourceRef, OperationError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| OperationError::Execution(error.to_string()))?;
    artifacts
        .put(
            &bytes,
            arsy_kernel::artifact::NewArtifact {
                media_type: "application/json".into(),
                creator,
                source_revision: None,
                sensitivity: arsy_kernel::artifact::Sensitivity::Internal,
                retain_until_ms,
            },
        )
        .map(|metadata| metadata.resource_ref())
        .map_err(|error| OperationError::Execution(error.to_string()))
}

/// How much of a result artifact may be read back. Larger than the text a
/// result can carry, so truncation is decided once, on the rendered text.
const ARTIFACT_LIMITS: ArtifactReadLimits = ArtifactReadLimits {
    max_bytes: 32 * 1024 * 1024,
    max_expansion_ratio: 1_000,
};

/// What a tool call did, in the one shape every caller reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResult {
    pub tool: String,
    pub success: bool,
    /// What the model is shown. On failure this is the error, phrased as what
    /// to do differently rather than only what went wrong.
    pub output: String,
    /// Workspace-relative paths this call wrote, moved, or removed.
    pub changed_files: Vec<String>,
    pub duration: Duration,
    /// The structured result, for callers that want more than the text.
    pub metadata: Value,
    /// Where the full result lives, when it was stored.
    ///
    /// The transcript carries a bounded rendering; this is how an elided
    /// observation is found again, so trimming the conversation loses nothing
    /// that cannot be fetched back.
    pub artifact: Option<ArtifactId>,
}

impl ToolResult {
    /// A call that did not run, and why.
    ///
    /// Public because a front end that asks the operator produces refusals of
    /// its own — a declined confirmation, an approval that could not be turned
    /// into a grant — and those have to reach the model in the same shape a
    /// tool failure does, not a second one each surface invents.
    pub fn refused(tool: &str, reason: impl Into<String>) -> Self {
        Self::failed(tool, reason, Duration::ZERO)
    }

    fn failed(tool: &str, output: impl Into<String>, duration: Duration) -> Self {
        Self {
            tool: tool.to_owned(),
            success: false,
            output: output.into(),
            changed_files: Vec::new(),
            duration,
            metadata: Value::Null,
            artifact: None,
        }
    }
}

/// What policy said about a call, before it runs.
#[derive(Clone, Debug)]
pub enum Authorization {
    /// Every requirement is covered; the grants are ready to dispatch with.
    Allowed(Vec<CapabilityGrant>),
    /// The operator has to answer before this can run.
    ///
    /// Every requirement still outstanding is carried, not just the first: a
    /// patch needs write *and* delete authority, and approving one of the two
    /// would leave the dispatch to fail on the other after the operator
    /// believed they had said yes.
    NeedsApproval {
        granted: Vec<CapabilityGrant>,
        approvals: Vec<ApprovalRequest>,
    },
    Denied(String),
}

impl Authorization {
    /// The grants an operator's "yes" produces, or why it cannot.
    pub fn approve(self) -> Result<Vec<CapabilityGrant>, String> {
        match self {
            Self::Allowed(grants) => Ok(grants),
            Self::NeedsApproval {
                mut granted,
                approvals,
            } => {
                for approval in approvals {
                    granted.push(approval.grant().map_err(|error| error.to_string())?);
                }
                Ok(granted)
            }
            Self::Denied(reason) => Err(reason),
        }
    }

    /// One line naming everything the operator is being asked to allow.
    pub fn requested(&self) -> String {
        match self {
            Self::NeedsApproval { approvals, .. } => approvals
                .iter()
                .map(|approval| approval.reason.clone())
                .collect::<Vec<_>>()
                .join("; "),
            Self::Allowed(_) => String::new(),
            Self::Denied(reason) => reason.clone(),
        }
    }
}

/// A tool as the model sees it, and how it becomes an operation.
pub struct Tool {
    pub name: &'static str,
    pub operation: &'static str,
    pub description: &'static str,
    schema: fn() -> Value,
    translate: fn(&Value) -> Result<Value, String>,
    summarize: fn(&Value) -> String,
}

impl Tool {
    pub fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.to_owned(),
            description: self.description.to_owned(),
            input_schema: (self.schema)(),
        }
    }
}

/// Default and ceiling for `bash`. The default is long enough for a test suite
/// and short enough that a hung command does not hold a turn open forever.
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn text(arguments: &Value, key: &str) -> String {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Every tool this build offers, in the order the model is shown them.
///
/// Read and search come first deliberately: a model shown `bash` at the top of
/// the list reaches for it, and a shelled-out `grep` is unbounded output, needs
/// execute authority to answer a read-only question, and cannot be replayed
/// from the audit trail.
pub const TOOLS: &[Tool] = &[
    Tool {
        name: "fs.read",
        operation: "fs.read",
        description: "Read a workspace file as text. Returns the file with its total line count. Use `offset` (one-based line) and `limit` to read part of a large file. Binary files are reported, not decoded.",
        schema: || {
            object(
                json!({
                    "path": {"type": "string", "description": "Workspace-relative path."},
                    "offset": {"type": "number", "description": "One-based first line. Defaults to 1."},
                    "limit": {"type": "number", "description": "How many lines to return. Defaults to the whole file."}
                }),
                &["path"],
            )
        },
        translate: |arguments| {
            let mut input = json!({"path": text(arguments, "path")});
            for key in ["offset", "limit"] {
                if let Some(value) = arguments.get(key).and_then(Value::as_u64) {
                    input[key] = json!(value);
                }
            }
            Ok(input)
        },
        summarize: |arguments| text(arguments, "path"),
    },
    Tool {
        name: "fs.list",
        operation: "fs.list",
        description: "List one directory of the workspace. Directories first, then files, both sorted.",
        schema: || {
            object(
                json!({"path": {"type": "string", "description": "Workspace-relative directory. Defaults to the root."}}),
                &[],
            )
        },
        translate: |arguments| {
            Ok(match arguments.get("path").and_then(Value::as_str) {
                Some(path) if !path.is_empty() => json!({"path": path}),
                _ => json!({}),
            })
        },
        summarize: |arguments| match arguments.get("path").and_then(Value::as_str) {
            Some(path) if !path.is_empty() => path.to_owned(),
            _ => ".".to_owned(),
        },
    },
    Tool {
        name: "search.files",
        operation: "search.files",
        description: "Find files by glob, honouring .gitignore. A pattern without a `/` also matches file names at any depth, so `*.rs` finds every Rust file.",
        schema: || {
            object(
                json!({
                    "pattern": {"type": "string", "description": "Glob, such as `*.rs` or `src/**/mod.rs`."},
                    "limit": {"type": "number", "description": "Most paths to return. Defaults to 100."}
                }),
                &["pattern"],
            )
        },
        translate: |arguments| {
            let mut input = json!({"pattern": text(arguments, "pattern")});
            if let Some(limit) = arguments.get("limit").and_then(Value::as_u64) {
                input["limit"] = json!(limit);
            }
            Ok(input)
        },
        summarize: |arguments| text(arguments, "pattern"),
    },
    Tool {
        name: "search.text",
        operation: "search.text",
        description: "Find a literal string in workspace files, honouring .gitignore and skipping binaries. Returns path, line number, and the matching line.",
        schema: || {
            object(
                json!({
                    "query": {"type": "string", "description": "Literal text to find. Not a regular expression."},
                    "limit": {"type": "number", "description": "Most matches to return. Defaults to 100."}
                }),
                &["query"],
            )
        },
        translate: |arguments| {
            let mut input = json!({"query": text(arguments, "query")});
            if let Some(limit) = arguments.get("limit").and_then(Value::as_u64) {
                input["limit"] = json!(limit);
            }
            Ok(input)
        },
        summarize: |arguments| text(arguments, "query"),
    },
    Tool {
        name: "code.symbol",
        operation: "code.symbol",
        description: "Find where a name is declared, using the repository's parsed declarations rather than a text match. Returns each declaration's file, byte range, and an id for `code.explain` and `code.references`. Falls back to a text search when nothing declares the name.",
        schema: || {
            object(
                json!({
                    "name": {"type": "string", "description": "Exact symbol name, such as `run` or `Workspace`."},
                    "limit": {"type": "number", "description": "Most declarations to return. Defaults to 20."}
                }),
                &["name"],
            )
        },
        translate: |arguments| {
            let mut input = json!({"name": text(arguments, "name")});
            if let Some(limit) = arguments.get("limit").and_then(Value::as_u64) {
                input["limit"] = json!(limit);
            }
            Ok(input)
        },
        summarize: |arguments| text(arguments, "name"),
    },
    Tool {
        name: "code.explain",
        operation: "code.explain",
        description: "Read one declaration by the id `code.symbol` returned: what kind it is, its source, and where it lives. Cheaper than reading the whole file it is in.",
        schema: || {
            object(
                json!({"symbol": {"type": "string", "description": "A symbol id from `code.symbol`, such as `symbol:src/lib.rs#run`."}}),
                &["symbol"],
            )
        },
        translate: |arguments| Ok(json!({"symbol": text(arguments, "symbol")})),
        summarize: |arguments| text(arguments, "symbol"),
    },
    Tool {
        name: "code.references",
        operation: "code.references",
        description: "Files that import the module a symbol is declared in — what a change to it could affect. An import is not proof of a call, and each result says how much it is worth.",
        schema: || {
            object(
                json!({"symbol": {"type": "string", "description": "A symbol id from `code.symbol`."}}),
                &["symbol"],
            )
        },
        translate: |arguments| Ok(json!({"symbol": text(arguments, "symbol")})),
        summarize: |arguments| text(arguments, "symbol"),
    },
    Tool {
        name: "code.diagnostics",
        operation: "code.diagnostics",
        description: "What the language server says is wrong with one file: the same errors and warnings a build would report, without running one. Only available where a language server is configured for the file type.",
        schema: || {
            object(
                json!({"path": {"type": "string", "description": "Workspace-relative path."}}),
                &["path"],
            )
        },
        translate: |arguments| Ok(json!({"path": text(arguments, "path")})),
        summarize: |arguments| text(arguments, "path"),
    },
    Tool {
        name: "code.rename",
        operation: "code.rename",
        description: "Rename a symbol everywhere the language server can prove it is used, in one transaction: either every file changes or none does. Takes a symbol id from `code.symbol`. Refused when no language server serves the file, because a rename that guesses is worse than no rename.",
        schema: || {
            object(
                json!({
                    "symbol": {"type": "string", "description": "A symbol id from `code.symbol`."},
                    "new_name": {"type": "string", "description": "The new name."}
                }),
                &["symbol", "new_name"],
            )
        },
        translate: |arguments| {
            Ok(json!({
                "symbol": text(arguments, "symbol"),
                "new_name": text(arguments, "new_name"),
            }))
        },
        summarize: |arguments| {
            format!(
                "{} → {}",
                text(arguments, "symbol"),
                text(arguments, "new_name")
            )
        },
    },
    Tool {
        name: "fs.edit",
        operation: "fs.edit",
        description: "Replace one occurrence of `old_text` with `new_text` in a file. `old_text` must appear exactly once unless `occurrence` selects which one; an ambiguous edit is refused rather than guessed. Read the file first.",
        schema: || {
            object(
                json!({
                    "path": {"type": "string", "description": "Workspace-relative path."},
                    "old_text": {"type": "string", "description": "Exact text to replace, including indentation."},
                    "new_text": {"type": "string", "description": "Replacement text."},
                    "occurrence": {"type": "number", "description": "One-based occurrence, when `old_text` appears more than once."}
                }),
                &["path", "old_text", "new_text"],
            )
        },
        translate: |arguments| {
            let mut input = json!({
                "path": text(arguments, "path"),
                "old_text": text(arguments, "old_text"),
                "new_text": text(arguments, "new_text"),
            });
            if let Some(occurrence) = arguments.get("occurrence").and_then(Value::as_u64) {
                input["occurrence"] = json!(occurrence);
            }
            Ok(input)
        },
        summarize: |arguments| text(arguments, "path"),
    },
    Tool {
        name: "apply_patch",
        operation: "fs.patch",
        description: concat!(
            "Edit several files with one patch. The patch is a string of the form:\n",
            "*** Begin Patch\n",
            "*** Update File: path/to/file.rs\n",
            "@@ optional context line\n",
            " unchanged line\n",
            "-removed line\n",
            "+added line\n",
            "*** End Patch\n",
            "`*** Add File: path` is followed by `+` lines only, ",
            "`*** Delete File: path` takes no body, and `*** Move to: path` ",
            "renames the file being updated. Paths are relative to the workspace."
        ),
        schema: || {
            object(
                json!({"patch": {"type": "string", "description": "The patch text, including the Begin/End markers."}}),
                &["patch"],
            )
        },
        translate: |arguments| Ok(json!({"patch": text(arguments, "patch")})),
        summarize: |arguments| {
            let patch = arguments
                .get("patch")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let files: Vec<&str> = patch
                .lines()
                .filter_map(|line| {
                    ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                        .iter()
                        .find_map(|marker| line.strip_prefix(marker))
                })
                .collect();
            if files.is_empty() {
                "<no files>".to_owned()
            } else {
                files.join(", ")
            }
        },
    },
    Tool {
        name: "fs.write",
        operation: "fs.write",
        description: "Write a whole file, creating it and any missing parent directory. Replaces the file entirely — prefer `fs.edit` or `apply_patch` for a change to an existing file.",
        schema: || {
            object(
                json!({
                    "path": {"type": "string", "description": "Workspace-relative path."},
                    "content": {"type": "string", "description": "The complete new contents."}
                }),
                &["path", "content"],
            )
        },
        translate: |arguments| {
            Ok(json!({
                "path": text(arguments, "path"),
                "content": text(arguments, "content"),
            }))
        },
        summarize: |arguments| text(arguments, "path"),
    },
    Tool {
        name: "fs.delete",
        operation: "fs.delete",
        description: "Remove a workspace file.",
        schema: || {
            object(
                json!({"path": {"type": "string", "description": "Workspace-relative path."}}),
                &["path"],
            )
        },
        translate: |arguments| Ok(json!({"path": text(arguments, "path")})),
        summarize: |arguments| text(arguments, "path"),
    },
    Tool {
        name: "fs.move",
        operation: "fs.move",
        description: "Rename or move a workspace file. Refuses to overwrite an existing destination.",
        schema: || {
            object(
                json!({
                    "from": {"type": "string", "description": "Current workspace-relative path."},
                    "to": {"type": "string", "description": "New workspace-relative path."}
                }),
                &["from", "to"],
            )
        },
        translate: |arguments| {
            Ok(json!({"from": text(arguments, "from"), "to": text(arguments, "to")}))
        },
        summarize: |arguments| format!("{} → {}", text(arguments, "from"), text(arguments, "to")),
    },
    Tool {
        name: "plugin.invoke",
        operation: "plugin.invoke",
        description: "Run an installed WASM plugin on a string and return what it produced. Only plugins this workspace installed and approved can run, and only within the limits the host enforces. Use `arsy plugin list` to see them.",
        schema: || {
            object(
                json!({
                    "plugin": {"type": "string", "description": "Installed plugin id."},
                    "input": {"type": "string", "description": "What the plugin is given. Defaults to empty."}
                }),
                &["plugin"],
            )
        },
        translate: |arguments| {
            let mut input = json!({"plugin": text(arguments, "plugin")});
            if let Some(text) = arguments.get("input").and_then(Value::as_str) {
                input["input"] = json!(text);
            }
            Ok(input)
        },
        summarize: |arguments| text(arguments, "plugin"),
    },
    Tool {
        name: "bash",
        operation: "process.exec",
        description: "Run a shell command in the workspace and return its merged stdout and stderr, truncated to the last 16 KiB, followed by an `evidence: <id>` line. Use the file and search tools for reading and editing; use this for builds, tests, and version control. Pass the evidence id to `validate_record` after a test, build, or lint run.",
        schema: || {
            object(
                json!({
                    "command": {"type": "string", "description": "Shell command to run."},
                    "timeout_ms": {"type": "number", "description": "Deadline in milliseconds. Defaults to 120000; capped at 600000."}
                }),
                &["command"],
            )
        },
        translate: |arguments| {
            let command = arguments
                .get("command")
                .and_then(Value::as_str)
                .filter(|command| !command.trim().is_empty())
                .ok_or("bash requires a non-empty `command` string")?;
            let timeout = arguments
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1, MAX_TIMEOUT_MS);
            Ok(json!({
                "argv": ["sh", "-c", command],
                "timeout_ms": timeout,
                // The executor bounds each stream; the runtime bounds the text
                // the model sees. This is the larger of the two, so a command's
                // tail survives to be truncated deliberately rather than cut
                // wherever the stream limit happened to fall.
                "max_output_bytes": 1_048_576,
            }))
        },
        summarize: |arguments| text(arguments, "command"),
    },
    Tool {
        name: "repo_discover",
        operation: "repo.discover",
        description: "Identify the repository: its git root, every manifest found (Cargo.toml, package.json, go.mod, ...) with the language it implies, and the members a workspace-level manifest declares. Call this before inferring the project's layout from `bash`.",
        schema: || object(json!({}), &[]),
        translate: |_| Ok(json!({})),
        summarize: |_| String::new(),
    },
    Tool {
        name: "git_status",
        operation: "git.status",
        description: "The working tree's status: cleanliness and every changed, added, deleted, renamed, or untracked file, each with its two-letter status code. Use this instead of `bash git status`.",
        schema: || object(json!({}), &[]),
        translate: |_| Ok(json!({})),
        summarize: |_| String::new(),
    },
    Tool {
        name: "git_branch",
        operation: "git.branch",
        description: "The current branch name. Empty when HEAD is detached.",
        schema: || object(json!({}), &[]),
        translate: |_| Ok(json!({})),
        summarize: |_| String::new(),
    },
    Tool {
        name: "git_diff",
        operation: "git.diff",
        description: "The diff against a revision (a commit, branch, or `HEAD`). Use this instead of `bash git diff`.",
        schema: || {
            object(
                json!({"revision": {"type": "string", "description": "A commit, branch, or ref, e.g. `HEAD` or `main`."}}),
                &["revision"],
            )
        },
        translate: |arguments| Ok(json!({"revision": text(arguments, "revision")})),
        summarize: |arguments| text(arguments, "revision"),
    },
    Tool {
        name: "git_log",
        operation: "git.log",
        description: "Recent commits: hash, author, date, and subject, one per line. Use this instead of `bash git log`.",
        schema: || {
            object(
                json!({"max_entries": {"type": "number", "description": "How many commits, most recent first. 1 to 1000."}}),
                &["max_entries"],
            )
        },
        translate: |arguments| {
            let max_entries = arguments
                .get("max_entries")
                .and_then(Value::as_u64)
                .ok_or("git_log requires a numeric `max_entries`")?;
            Ok(json!({"max_entries": max_entries}))
        },
        summarize: |arguments| {
            arguments
                .get("max_entries")
                .map(|value| value.to_string())
                .unwrap_or_default()
        },
    },
    Tool {
        name: "git_blame",
        operation: "git.blame",
        description: "Per-line authorship for one workspace file. Use this instead of `bash git blame`.",
        schema: || {
            object(
                json!({"path": {"type": "string", "description": "Workspace-relative path."}}),
                &["path"],
            )
        },
        translate: |arguments| Ok(json!({"path": text(arguments, "path")})),
        summarize: |arguments| text(arguments, "path"),
    },
    Tool {
        name: "plan_add",
        operation: "plan.add",
        description: "Add a step to the task's plan. Returns the whole plan. Steps start `pending`; put a new one after an existing step with `after`, or leave it off to append.",
        schema: || {
            object(
                json!({
                    "description": {"type": "string", "description": "What the step is."},
                    "after": {"type": "string", "description": "Step id to insert after. Defaults to the end of the plan."}
                }),
                &["description"],
            )
        },
        translate: |arguments| {
            let mut input = json!({"description": text(arguments, "description")});
            if let Some(after) = arguments.get("after").and_then(Value::as_str) {
                input["after"] = json!(after);
            }
            Ok(input)
        },
        summarize: |arguments| text(arguments, "description"),
    },
    Tool {
        name: "plan_update",
        operation: "plan.update",
        description: "Change a plan step's status or description. Status is one of `pending`, `in_progress`, `completed`. Returns the whole plan.",
        schema: || {
            object(
                json!({
                    "id": {"type": "string", "description": "Step id, as returned by plan_add or plan_list."},
                    "status": {"type": "string", "description": "pending | in_progress | completed"},
                    "description": {"type": "string", "description": "Replacement text. Leave unset to keep it."}
                }),
                &["id"],
            )
        },
        translate: |arguments| {
            let mut input = json!({"id": text(arguments, "id")});
            for key in ["status", "description"] {
                if let Some(value) = arguments.get(key).and_then(Value::as_str) {
                    input[key] = json!(value);
                }
            }
            Ok(input)
        },
        summarize: |arguments| text(arguments, "id"),
    },
    Tool {
        name: "plan_remove",
        operation: "plan.remove",
        description: "Remove a step from the plan. Returns the whole plan.",
        schema: || {
            object(
                json!({"id": {"type": "string", "description": "Step id to remove."}}),
                &["id"],
            )
        },
        translate: |arguments| Ok(json!({"id": text(arguments, "id")})),
        summarize: |arguments| text(arguments, "id"),
    },
    Tool {
        name: "plan_reorder",
        operation: "plan.reorder",
        description: "Put the plan's steps in a new order. `order` must name every current step id exactly once.",
        schema: || {
            object(
                json!({
                    "order": {"type": "array", "items": {"type": "string"}, "description": "Every step id, in the new order."}
                }),
                &["order"],
            )
        },
        translate: |arguments| {
            let order = arguments
                .get("order")
                .and_then(Value::as_array)
                .ok_or("plan_reorder requires an `order` array")?;
            Ok(json!({"order": order}))
        },
        summarize: |_| "reorder".to_owned(),
    },
    Tool {
        name: "plan_list",
        operation: "plan.list",
        description: "Read the current plan back without changing it.",
        schema: || object(json!({}), &[]),
        translate: |_| Ok(json!({})),
        summarize: |_| String::new(),
    },
    Tool {
        name: "validate_record",
        operation: "validate.record",
        description: "Record a check you just ran with `bash` (a test suite, a build, a linter) against the task. `evidence` is the id from the `evidence: <id>` line `bash`'s own result ended with — the outcome is read from that command's real exit code, not from what you say it was. This is what a completion claim cites.",
        schema: || {
            object(
                json!({
                    "command": {"type": "string", "description": "The command that was run."},
                    "evidence": {"type": "string", "description": "The id from the `evidence:` line at the end of that command's `bash` result."},
                    "detail": {"type": "string", "description": "The failing assertion or a short summary. Optional."}
                }),
                &["command", "evidence"],
            )
        },
        translate: |arguments| {
            let mut input = json!({
                "command": text(arguments, "command"),
                "evidence": text(arguments, "evidence"),
            });
            if let Some(detail) = arguments.get("detail").and_then(Value::as_str) {
                input["detail"] = json!(detail);
            }
            Ok(input)
        },
        summarize: |arguments| text(arguments, "command"),
    },
    Tool {
        name: "validate_status",
        operation: "validate.status",
        description: "Read the validation log back without changing it. The task is only done once the last entry passed.",
        schema: || object(json!({}), &[]),
        translate: |_| Ok(json!({})),
        summarize: |_| String::new(),
    },
];

pub fn tool(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|tool| tool.name == name)
}

/// Executes tool calls for one workspace under one policy.
///
/// Cloneable and cheap to share: everything it owns is either a handle or a
/// small value, so the TUI and a background turn can hold the same runtime.
#[derive(Clone)]
pub struct ToolRuntime {
    registry: Arc<OperationRegistry>,
    artifacts: Arc<dyn ArtifactStore>,
    rules: Arc<RuleSet>,
    workspace: PathBuf,
    actor: Principal,
    context: RiskContext,
}

impl ToolRuntime {
    pub fn new(
        registry: OperationRegistry,
        rules: RuleSet,
        artifacts: Arc<dyn ArtifactStore>,
        workspace: impl AsRef<Path>,
        actor: Principal,
        context: RiskContext,
    ) -> Self {
        Self {
            registry: Arc::new(registry),
            artifacts,
            rules: Arc::new(rules),
            workspace: workspace.as_ref().to_path_buf(),
            actor,
            context,
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Install a bounded live-output sink for long-running executors.
    pub fn set_output_sink(&self, sink: Option<arsy_kernel::operation::OutputSink>) {
        self.registry.set_output_sink(sink);
    }

    /// The tools this build can actually dispatch.
    ///
    /// Filtered by the registry rather than listed statically: offering a tool
    /// whose operation is not registered invites a call nothing can run.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        TOOLS
            .iter()
            .filter(|tool| self.registered(tool))
            .map(Tool::schema)
            .collect()
    }

    fn registered(&self, tool: &Tool) -> bool {
        OperationKind::new(tool.operation)
            .ok()
            .is_some_and(|kind| self.registry.contract(&kind).is_some())
    }

    /// A one-line description of a call, for the row shown before it runs.
    pub fn summarize(&self, name: &str, arguments: &Value) -> String {
        match tool(name) {
            Some(tool) => (tool.summarize)(arguments),
            None => name.to_owned(),
        }
    }

    /// Turn a model call into a request, or say why it cannot be one.
    fn decode(&self, name: &str, arguments: &Value) -> Result<OperationRequest, String> {
        let tool = tool(name).ok_or_else(|| {
            let offered = self
                .schemas()
                .iter()
                .map(|schema| schema.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            format!("`{name}` is not a tool this session offers; the tools are: {offered}")
        })?;
        if !self.registered(tool) {
            return Err(format!(
                "`{name}` is not available in this workspace: no executor is registered for {}",
                tool.operation
            ));
        }
        let input = (tool.translate)(arguments)?;
        let kind = OperationKind::new(tool.operation).expect("a static operation kind is valid");
        let contract = self
            .registry
            .contract(&kind)
            .expect("registration was just checked");
        Ok(OperationRequest {
            id: OperationId::new(),
            kind,
            actor: self.actor.clone(),
            requirements: crate::operations::requirements(contract, &input, &self.workspace),
            input,
        })
    }

    /// Ask policy about a call without running it.
    ///
    /// Separate from [`Self::invoke`] so a front end can show the operator what
    /// is being asked for, and so a refusal costs nothing.
    /// The grants this actor holds outright over the whole workspace, for the
    /// actions asked about.
    ///
    /// Delegation needs a grant to attenuate from, and a grant is normally
    /// minted per call. A supervisor about to hand authority to a child has no
    /// call yet — it has a decision to make about what the child may do at all —
    /// so this asks the same engine the same question at workspace scope and
    /// keeps only what came back allowed.
    ///
    /// Anything policy would merely have asked about is left out: an approval
    /// is an answer from an operator about one operation, and it cannot be
    /// spent on a child's future calls.
    pub fn delegable_grants(&self, actions: &[CapabilityAction]) -> Vec<CapabilityGrant> {
        actions
            .iter()
            .filter_map(|action| {
                let requirement = CapabilityRequirement {
                    action: *action,
                    resource: arsy_kernel::domain::ResourceRef::new(action.default_scheme(), "**")
                        .ok()?,
                };
                let query = PolicyQuery {
                    actor: self.actor.clone(),
                    operation: OperationKind::new("task.spawn").ok()?,
                    requirement,
                    operation_digest: arsy_kernel::domain::StateVersion::from_digest([0; 32]),
                    resource_version: None,
                    // Asked as a question about authority, not about an effect.
                    // Policy raises an irreversible *call* to approval, and
                    // delegating is not a call: the child's own runtime
                    // evaluates each of its operations on that operation's own
                    // contract, and an irreversible one is escalated there,
                    // where an operator could actually be asked about it.
                    context: RiskContext {
                        reversible: true,
                        ..self.context
                    },
                };
                match self.rules.evaluate(&query).decision {
                    PolicyDecision::Allow(grant) => Some(grant),
                    _ => None,
                }
            })
            .collect()
    }

    pub fn authorize(&self, request: &OperationRequest) -> Authorization {
        let digest = request.digest();
        // Reversibility is the operation's own claim, not a guess from the
        // transport: policy raises an irreversible call to approval, and taking
        // the answer from the contract is what makes an approval mean the same
        // thing here, in `arsy serve`, and in `arsy policy explain`.
        let reversible = self
            .registry
            .contract(&request.kind)
            .is_some_and(|contract| contract.reversible);
        let context = RiskContext {
            reversible,
            ..self.context
        };
        let mut granted = Vec::with_capacity(request.requirements.len());
        let mut approvals = Vec::new();
        for requirement in &request.requirements {
            let query = PolicyQuery {
                actor: request.actor.clone(),
                operation: request.kind.clone(),
                requirement: requirement.clone(),
                operation_digest: digest,
                resource_version: None,
                context,
            };
            match self.rules.evaluate(&query).decision {
                PolicyDecision::Allow(grant) => granted.push(grant),
                PolicyDecision::RequireApproval(approval) => approvals.push(approval),
                // Refusal is total and immediate: dispatching the rest would
                // perform part of an operation policy refused.
                PolicyDecision::Deny(reason) => {
                    return Authorization::Denied(describe(requirement, &reason.message))
                }
            }
        }
        if approvals.is_empty() {
            Authorization::Allowed(granted)
        } else {
            Authorization::NeedsApproval { granted, approvals }
        }
    }

    /// Run one call end to end, deciding authority itself.
    ///
    /// A call needing approval is refused here rather than run: a caller that
    /// can ask the operator uses [`Self::authorize`] first and then
    /// [`Self::dispatch`] with the grants it was given.
    pub fn invoke(&self, name: &str, arguments: &Value) -> ToolResult {
        let started = Instant::now();
        let request = match self.decode(name, arguments) {
            Ok(request) => request,
            Err(error) => return ToolResult::failed(name, error, started.elapsed()),
        };
        let authorization = self.authorize(&request);
        match authorization {
            Authorization::Allowed(grants) => self.dispatch(name, &request, &grants, started),
            Authorization::NeedsApproval { .. } => ToolResult::failed(
                name,
                format!(
                    "this call needs the operator's approval, and this surface cannot ask for one: {}",
                    authorization.requested()
                ),
                started.elapsed(),
            ),
            Authorization::Denied(reason) => ToolResult::failed(name, reason, started.elapsed()),
        }
    }

    /// Decode a call so a caller can inspect and authorize it before running.
    ///
    /// The failure is boxed because it is the same [`ToolResult`] a successful
    /// call returns — the caller needs no second error shape, and a decode
    /// failure is rare enough that a pointer costs nothing.
    pub fn prepare(
        &self,
        name: &str,
        arguments: &Value,
    ) -> Result<OperationRequest, Box<ToolResult>> {
        self.decode(name, arguments)
            .map_err(|error| Box::new(ToolResult::failed(name, error, Duration::ZERO)))
    }

    /// Dispatch an already-authorized request.
    pub fn dispatch(
        &self,
        name: &str,
        request: &OperationRequest,
        grants: &[CapabilityGrant],
        started: Instant,
    ) -> ToolResult {
        match self.registry.dispatch(request, grants, unix_time_ms()) {
            Ok(outcome) => self.render(name, &outcome, started.elapsed()),
            // A tool that ran and failed is a result, not a crash: the model
            // gets the reason and can choose something else.
            Err(error) => ToolResult::failed(name, explain(&error), started.elapsed()),
        }
    }

    /// Read the result artifact back and render it as text for the model.
    ///
    /// Executors store their result rather than returning it inline, which is
    /// what makes every tool call replayable from the artifact store. That is
    /// the right shape for an audit trail and the wrong one for a turn, so the
    /// bridge is here, in one place, instead of in each executor.
    fn render(&self, name: &str, outcome: &OperationOutcome, duration: Duration) -> ToolResult {
        let Some(reference) = &outcome.value else {
            return ToolResult {
                tool: name.to_owned(),
                success: true,
                output: "(no output)".to_owned(),
                changed_files: Vec::new(),
                duration,
                metadata: Value::Null,
                artifact: None,
            };
        };
        let artifact: Option<ArtifactId> = reference.value().parse().ok();
        let value = match reference
            .value()
            .parse()
            .map_err(|_| "result reference is not an artifact id".to_owned())
            .and_then(|id| {
                self.artifacts
                    .read(id, ARTIFACT_LIMITS)
                    .map_err(|error| error.to_string())
            })
            .and_then(|bytes| {
                serde_json::from_slice::<Value>(&bytes).map_err(|error| error.to_string())
            }) {
            Ok(value) => value,
            Err(error) => {
                return ToolResult::failed(
                    name,
                    format!("the tool ran but its result could not be read back: {error}"),
                    duration,
                )
            }
        };
        let (success, output) = present(name, &value, &self.evidence(outcome));
        // `bash` is the one tool `validate_record` needs an id back from: its
        // artifact is what `validate.record` reads to find the real exit
        // code, so the model has to be able to name it. Nothing else reads
        // this line; appending it elsewhere would just be noise.
        let output = match (name, artifact) {
            ("bash", Some(id)) => format!("{output}\n\nevidence: {id}"),
            _ => output,
        };
        ToolResult {
            tool: name.to_owned(),
            success,
            output: truncate(&output),
            changed_files: changed(&value),
            duration,
            metadata: value,
            artifact,
        }
    }

    /// `process.exec` keeps stdout and stderr as evidence artifacts, because a
    /// command's output is often larger than its result. Reading them here is
    /// what turns "exit code 1" into something a model can act on.
    fn evidence(&self, outcome: &OperationOutcome) -> Vec<String> {
        outcome
            .evidence
            .iter()
            .map(|reference| {
                reference
                    .value()
                    .parse()
                    .ok()
                    .and_then(|id| self.artifacts.read(id, ARTIFACT_LIMITS).ok())
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default()
            })
            .collect()
    }
}

/// Render one operation's result as the text the model reads.
fn present(name: &str, value: &Value, evidence: &[String]) -> (bool, String) {
    match name {
        "bash" => {
            let mut merged = evidence.join("");
            if merged.trim().is_empty() {
                merged = "(no output)".to_owned();
            }
            let code = value.get("status_code").and_then(Value::as_i64);
            if value.get("timed_out").and_then(Value::as_bool) == Some(true) {
                return (
                    false,
                    format!("{merged}\n\nCommand exceeded its deadline and was stopped"),
                );
            }
            match code {
                Some(0) => (true, merged),
                Some(code) => (
                    false,
                    format!("{merged}\n\nCommand exited with code {code}"),
                ),
                None => (
                    false,
                    format!("{merged}\n\nCommand was terminated by a signal"),
                ),
            }
        }
        "fs.read" => {
            if value.get("binary").and_then(Value::as_bool) == Some(true) {
                return (
                    true,
                    format!(
                        "{} is a binary file; it was not decoded.",
                        value
                            .get("path")
                            .and_then(Value::as_str)
                            .unwrap_or("the file")
                    ),
                );
            }
            let first = value.get("first_line").and_then(Value::as_u64).unwrap_or(1);
            let returned = value
                .get("lines_returned")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let total = value
                .get("total_lines")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let body = value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // Nothing came back, so say why rather than returning an empty
            // string a model would read as an empty file. The two causes are
            // different questions, and only one of them has a next step.
            if returned == 0 {
                return (
                    true,
                    if total == 0 {
                        "(empty file)".to_owned()
                    } else {
                        format!("(offset {first} is past the end; the file has {total} lines)")
                    },
                );
            }
            let header = if returned < total {
                format!("lines {first}-{} of {total}\n", first + returned - 1)
            } else {
                String::new()
            };
            (true, format!("{header}{body}"))
        }
        "fs.list" => {
            let entries = value
                .get("entries")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .map(|entry| {
                            let name = entry.get("name").and_then(Value::as_str).unwrap_or("?");
                            if entry.get("directory").and_then(Value::as_bool) == Some(true) {
                                format!("{name}/")
                            } else {
                                format!(
                                    "{name} ({} bytes)",
                                    entry.get("bytes").and_then(Value::as_u64).unwrap_or(0)
                                )
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if entries.is_empty() {
                (true, "(empty directory)".to_owned())
            } else {
                (true, entries.join("\n"))
            }
        }
        "search.text" => {
            let hits: Vec<String> = value
                .get("hits")
                .and_then(Value::as_array)
                .map(|hits| {
                    hits.iter()
                        .map(|hit| {
                            format!(
                                "{}:{}: {}",
                                hit.get("path").and_then(Value::as_str).unwrap_or("?"),
                                hit.get("line").and_then(Value::as_u64).unwrap_or(0),
                                hit.get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .trim()
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            (true, listing(hits, value, "no matches"))
        }
        "search.files" => {
            let paths: Vec<String> = value
                .get("paths")
                .and_then(Value::as_array)
                .map(|paths| {
                    paths
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            (true, listing(paths, value, "no files matched"))
        }
        "apply_patch" => {
            let summary: Vec<String> = value
                .get("summary")
                .and_then(Value::as_array)
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            (true, summary.join("\n"))
        }
        "fs.write" | "fs.edit" => {
            let path = value
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("the file");
            let verb = if value.get("created").and_then(Value::as_bool) == Some(true) {
                "created"
            } else {
                "updated"
            };
            (
                true,
                format!(
                    "{verb} {path} ({} bytes)",
                    value.get("bytes").and_then(Value::as_u64).unwrap_or(0)
                ),
            )
        }
        "fs.delete" => (
            true,
            format!(
                "deleted {}",
                value
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("the file")
            ),
        ),
        "fs.move" => (
            true,
            format!(
                "moved {} to {}",
                value.get("from").and_then(Value::as_str).unwrap_or("?"),
                value.get("to").and_then(Value::as_str).unwrap_or("?")
            ),
        ),
        _ => (
            true,
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
        ),
    }
}

fn listing(lines: Vec<String>, value: &Value, empty: &str) -> String {
    if lines.is_empty() {
        return empty.to_owned();
    }
    let mut rendered = lines.join("\n");
    if value.get("truncated").and_then(Value::as_bool) == Some(true) {
        rendered.push_str("\n\n(more results were available; narrow the query or raise `limit`)");
    }
    rendered
}

/// The workspace paths an operation's result says it changed.
fn changed(value: &Value) -> Vec<String> {
    if let Some(paths) = value.get("changed").and_then(Value::as_array) {
        return paths
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }
    // A single-file result names its own path, and a move names both ends.
    ["path", "from", "to"]
        .iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// Keep the last [`MAX_TOOL_OUTPUT_BYTES`], cut at a character boundary.
///
/// The tail rather than the head: a failing command says why at the end.
fn truncate(text: &str) -> String {
    if text.len() <= MAX_TOOL_OUTPUT_BYTES {
        return text.to_owned();
    }
    let mut cut = text.len() - MAX_TOOL_OUTPUT_BYTES;
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!(
        "[{cut} earlier bytes omitted]\n{}",
        text.get(cut..).unwrap_or_default()
    )
}

fn describe(requirement: &CapabilityRequirement, reason: &str) -> String {
    format!(
        "denied by policy: {} over {}:{} — {reason}",
        requirement.action,
        requirement.resource.scheme(),
        requirement.resource.value()
    )
}

/// An operation failure as advice.
fn explain(error: &OperationError) -> String {
    match error {
        OperationError::Ungranted(requirement) => format!(
            "no capability grant covers {} over {}:{}",
            requirement.action,
            requirement.resource.scheme(),
            requirement.resource.value()
        ),
        other => other.to_string(),
    }
}

/// Build the runtime for one workspace, with every operation this build offers.
pub fn runtime(
    workspace: &Workspace,
    rules: RuleSet,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
    actor: Principal,
    context: RiskContext,
    reachable: crate::operations::Reachable,
) -> Result<ToolRuntime, arsy_kernel::operation::RegistrationError> {
    let registry = crate::operations::registry(
        workspace,
        Arc::clone(&artifacts),
        retain_until_ms,
        reachable,
    )?;
    Ok(ToolRuntime::new(
        registry,
        rules,
        artifacts,
        workspace.path(),
        actor,
        context,
    ))
}
