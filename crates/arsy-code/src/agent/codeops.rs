//! Semantic navigation as operations: find a declaration, read it, find what
//! could be affected by changing it.
//!
//! # Why this is not `search.text`
//!
//! `search.text` finds a string. Asked for `run`, it returns the comment that
//! mentions it, the local variable that shares its name, and the declaration,
//! and the model has to spend a turn reading files to tell them apart. These
//! operations answer the question the model actually had — *where is this
//! defined, what is it, and what would I break* — using whatever evidence the
//! workspace can support, and they say which tier answered so a caller can
//! weigh it.
//!
//! # Tiers
//!
//! ```text
//! language server   proves what a name binds to        (when configured)
//! tree-sitter graph knows a declaration from a mention (any Rust workspace)
//! text             finds the string                    (last resort)
//! ```
//!
//! The tier is chosen per call and reported in the result. Nothing here is
//! mandatory: a workspace with no Rust and no server still answers, at the
//! confidence that evidence deserves.

use crate::{
    edit::{self, EditAddress, EditOperation, RangeReplacement},
    intelligence::{
        CodeIntelligence, GraphCodeIntelligence, IntelligenceError, LspCodeIntelligence, SymbolId,
        SymbolQuery, TextCodeIntelligence, WorkspaceEditPlan, MAX_SEMANTIC_RESULTS,
    },
    lsp::{CommandOrigin, LspHost, RestartPolicy, ServerCommand, StdioTransport},
    resource::Workspace,
};
use arsy_kernel::{
    artifact::ArtifactStore,
    capability::{CapabilityAction, CapabilityGrant},
    domain::ResourceRef,
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
};
use arsy_kernel::{config::LanguageServer, domain::StateVersion};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

const DEFAULT_LIMIT: u64 = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodeOperation {
    /// Where a name is declared.
    Symbol,
    /// What one declaration is.
    Explain,
    /// What could be affected by changing it.
    References,
    /// What a language server says is wrong with a file.
    Diagnostics,
    /// Rename a symbol everywhere the server can prove it is used.
    Rename,
}

impl CodeOperation {
    pub const ALL: [Self; 5] = [
        Self::Symbol,
        Self::Explain,
        Self::References,
        Self::Diagnostics,
        Self::Rename,
    ];

    const fn kind(self) -> &'static str {
        match self {
            Self::Symbol => "code.symbol",
            Self::Explain => "code.explain",
            Self::References => "code.references",
            Self::Diagnostics => "code.diagnostics",
            Self::Rename => "code.rename",
        }
    }

    /// A rename writes; everything else reads.
    fn actions(self) -> Vec<CapabilityAction> {
        match self {
            Self::Rename => vec![CapabilityAction::FsRead, CapabilityAction::FsWrite],
            _ => vec![CapabilityAction::FsRead],
        }
    }

    fn schema(self) -> InputSchema {
        match self {
            Self::Symbol => InputSchema {
                required: BTreeMap::from([("name".to_owned(), JsonType::String)]),
                optional: BTreeMap::from([
                    ("limit".to_owned(), JsonType::Number),
                    ("tier".to_owned(), JsonType::String),
                ]),
                allow_extra: false,
            },
            Self::Explain | Self::References => InputSchema {
                required: BTreeMap::from([("symbol".to_owned(), JsonType::String)]),
                optional: BTreeMap::new(),
                allow_extra: false,
            },
            Self::Diagnostics => InputSchema {
                required: BTreeMap::from([("path".to_owned(), JsonType::String)]),
                optional: BTreeMap::new(),
                allow_extra: false,
            },
            Self::Rename => InputSchema {
                required: BTreeMap::from([
                    ("symbol".to_owned(), JsonType::String),
                    ("new_name".to_owned(), JsonType::String),
                ]),
                optional: BTreeMap::new(),
                allow_extra: false,
            },
        }
    }
}

/// What a semantic call answered, and on what evidence.
#[derive(Debug, Serialize)]
struct CodeOutcome {
    /// `lsp`, `syntax`, or `text`: which tier answered.
    provider: String,
    #[serde(flatten)]
    answer: Value,
}

pub struct CodeExecutor {
    operation: CodeOperation,
    contract: OperationContract,
    workspace: PathBuf,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
    /// Language servers configuration allows this workspace to start. Empty is
    /// the ordinary case, and the tiers below still answer.
    servers: Vec<LanguageServer>,
}

impl CodeExecutor {
    pub fn new(
        operation: CodeOperation,
        workspace: &Workspace,
        artifacts: Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
        servers: Vec<LanguageServer>,
    ) -> Arc<Self> {
        Arc::new(Self {
            operation,
            contract: OperationContract {
                kind: OperationKind::new(operation.kind()).expect("static operation kind is valid"),
                input_schema: operation.schema(),
                // Reading the repository, however cleverly. A semantic answer
                // needs no authority a file read does not; a rename writes, and
                // says so.
                actions: operation.actions(),
                idempotency: if operation == CodeOperation::Rename {
                    Idempotency::Effectful
                } else {
                    Idempotency::Idempotent
                },
                reversible: true,
                concurrency: if operation == CodeOperation::Rename {
                    ConcurrencyRule::ExclusiveGlobal
                } else {
                    ConcurrencyRule::Parallel
                },
            },
            workspace: workspace.path().to_owned(),
            artifacts,
            retain_until_ms,
            servers,
        })
    }
}

pub fn executors(
    workspace: &Workspace,
    artifacts: &Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
    servers: Vec<LanguageServer>,
) -> Vec<Arc<dyn OperationExecutor>> {
    CodeOperation::ALL
        .into_iter()
        .map(|operation| {
            CodeExecutor::new(
                operation,
                workspace,
                Arc::clone(artifacts),
                retain_until_ms,
                servers.clone(),
            ) as Arc<dyn OperationExecutor>
        })
        .collect()
}

impl CodeExecutor {
    /// A language server for this file, started and ready to be asked.
    ///
    /// `None` when configuration named none for the extension, which is the
    /// ordinary case: the tiers below still answer, at the confidence their
    /// evidence deserves.
    fn language_server<'a>(
        &self,
        workspace: &'a Workspace,
        path: &Path,
    ) -> Option<LspCodeIntelligence<'a, StdioTransport>> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        let server = self
            .servers
            .iter()
            .find(|server| server.extensions.contains(&extension))?;
        Some(self.start(workspace, server))
    }

    /// The first configured server, for a question about no file in
    /// particular. `workspace/symbol` is workspace-wide, so there is no
    /// extension to choose by.
    fn any_language_server<'a>(
        &self,
        workspace: &'a Workspace,
    ) -> Option<LspCodeIntelligence<'a, StdioTransport>> {
        self.servers
            .first()
            .map(|server| self.start(workspace, server))
    }

    fn start<'a>(
        &self,
        workspace: &'a Workspace,
        server: &LanguageServer,
    ) -> LspCodeIntelligence<'a, StdioTransport> {
        let transport = StdioTransport::new(
            // The same allowlist a subprocess gets: a language server is a
            // program, and a credential in the operator's shell has no reason
            // to be in its environment.
            crate::operations::DEFAULT_ENVIRONMENT_ALLOWLIST
                .iter()
                .filter_map(|name| {
                    std::env::var(name)
                        .ok()
                        .map(|value| ((*name).to_owned(), value))
                })
                .collect(),
            Some(workspace.path().to_owned()),
        )
        .rooted_at(workspace.path());
        LspCodeIntelligence::new(
            server.name.clone(),
            // Replaced with a digest of the files a plan touches before that
            // plan is applied; nothing reads it before then.
            StateVersion::from_digest([0; 32]),
            LspHost::new(
                ServerCommand {
                    argv: server.command.clone(),
                    // Configuration that may name a program comes from a
                    // trusted layer, which is the same fact `Installed` states.
                    origin: CommandOrigin::Installed,
                    policy_authorized: true,
                },
                transport,
                RestartPolicy::default(),
            ),
            workspace,
        )
    }
}

impl OperationExecutor for CodeExecutor {
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
        let text = |key: &str| text(&request.input, key);

        let outcome = match self.operation {
            CodeOperation::Symbol => {
                let query = SymbolQuery {
                    name: text("name"),
                    max_results: usize::try_from(
                        request
                            .input
                            .get("limit")
                            .and_then(Value::as_u64)
                            .unwrap_or(DEFAULT_LIMIT),
                    )
                    .unwrap_or(MAX_SEMANTIC_RESULTS)
                    .clamp(1, MAX_SEMANTIC_RESULTS),
                };
                // Down the tiers until one has an answer. A server that is
                // configured but cannot start is a fallback, not a failure:
                // the question is still answerable, less certainly.
                //
                // `tier` pins the answer to one of them. Nothing in a turn
                // asks for that; a measurement does, because "the semantic
                // answer is better than the textual one" is a claim that needs
                // both answers to exist side by side.
                let floor = text("tier") == "text";
                let hits = if floor {
                    TextCodeIntelligence::new(&workspace)
                        .find_symbol(&query)
                        .map_err(semantic)?
                } else {
                    self.any_language_server(&workspace)
                        .and_then(|mut server| server.find_symbol(&query).ok())
                        .filter(|hits| !hits.is_empty())
                        .or_else(|| {
                            GraphCodeIntelligence::index(&workspace)
                                .and_then(|mut graph| graph.find_symbol(&query))
                                .ok()
                                .filter(|hits| !hits.is_empty())
                        })
                        .map_or_else(
                            || TextCodeIntelligence::new(&workspace).find_symbol(&query),
                            Ok,
                        )
                        .map_err(semantic)?
                };
                CodeOutcome {
                    provider: provider_of(hits.first().map(|hit| hit.provider)),
                    answer: serde_json::json!({"symbols": hits}),
                }
            }
            CodeOperation::Explain => {
                let id = symbol(&text("symbol"))?;
                let evidence = self
                    .tier_for(&workspace, &id)?
                    .explain_symbol(&id)
                    .map_err(semantic)?;
                CodeOutcome {
                    provider: provider_of(Some(evidence.provider)),
                    answer: serde_json::to_value(evidence)
                        .map_err(|error| OperationError::Execution(error.to_string()))?,
                }
            }
            CodeOperation::References => {
                let id = symbol(&text("symbol"))?;
                let found = self
                    .tier_for(&workspace, &id)?
                    .find_callers(&id)
                    .map_err(semantic)?;
                CodeOutcome {
                    provider: provider_of(found.callers.first().map(|hit| hit.provider)),
                    answer: serde_json::to_value(found)
                        .map_err(|error| OperationError::Execution(error.to_string()))?,
                }
            }
            CodeOperation::Diagnostics => {
                let path = PathBuf::from(text("path"));
                let found = self
                    .language_server(&workspace, &path)
                    .ok_or_else(|| unserved(&path))?
                    .diagnostics(&text("path"))
                    .map_err(semantic)?;
                CodeOutcome {
                    provider: provider_of(Some(found.provider)),
                    answer: serde_json::to_value(found)
                        .map_err(|error| OperationError::Execution(error.to_string()))?,
                }
            }
            CodeOperation::Rename => {
                let id = symbol(&text("symbol"))?;
                let path = lsp_path(&id, &workspace)?;
                let mut server = self
                    .language_server(&workspace, &path)
                    .ok_or_else(|| unserved(&path))?;
                let plan = server
                    .plan_rename(&id, &text("new_name"))
                    .map_err(semantic)?;
                let applied = apply_rename(&workspace, plan)?;
                CodeOutcome {
                    provider: "lsp".to_owned(),
                    answer: applied,
                }
            }
        };

        let value = super::store(
            self.artifacts.as_ref(),
            &outcome,
            request.actor.clone(),
            self.retain_until_ms,
        )?;
        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: if self.operation == CodeOperation::Rename {
                    CapabilityAction::FsWrite
                } else {
                    CapabilityAction::FsRead
                },
                resource: ResourceRef::new("workspace", "*").expect("a static scheme and value"),
            }],
            evidence: Vec::new(),
            state: None,
        })
    }
}

impl CodeExecutor {
    /// The tier that can answer about this id.
    ///
    /// An id says which tier produced it — `lsp:` carries a position a server
    /// understands, `symbol:` names a declaration the graph indexed — so
    /// routing by prefix asks the tier that can actually resolve it rather
    /// than the one that happens to be available.
    fn tier_for<'a>(
        &self,
        workspace: &'a Workspace,
        id: &SymbolId,
    ) -> Result<Box<dyn CodeIntelligence + 'a>, OperationError> {
        if id.as_str().starts_with("lsp:") {
            let path = lsp_path(id, workspace)?;
            return self
                .language_server(workspace, &path)
                .map(|server| Box::new(server) as Box<dyn CodeIntelligence + 'a>)
                .ok_or_else(|| unserved(&path));
        }
        GraphCodeIntelligence::index(workspace)
            .map(|graph| Box::new(graph) as Box<dyn CodeIntelligence + 'a>)
            .map_err(semantic)
    }
}

/// One string field of a call's input, or the empty string.
fn text(input: &Value, key: &str) -> String {
    input
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The workspace-relative file an `lsp:` id names.
fn lsp_path(id: &SymbolId, workspace: &Workspace) -> Result<PathBuf, OperationError> {
    let uri = id
        .as_str()
        .strip_prefix("lsp:")
        .and_then(|rest| rest.rsplit_once('#'))
        .map(|(uri, _)| uri)
        .ok_or_else(|| {
            OperationError::Execution(format!(
                "`{}` is not a symbol id from a language server; find one with code.symbol",
                id.as_str()
            ))
        })?;
    let path = crate::lsp::uri_path(uri);
    Ok(path
        .strip_prefix(workspace.path())
        .unwrap_or(&path)
        .to_path_buf())
}

/// Apply a rename plan as one transaction.
///
/// The plan carries a digest of the documents its offsets were computed
/// against. This re-reads those files from disk and computes the same digest;
/// `EditAddress::WorkspaceEdit` refuses unless the two match, so a file that
/// moved between planning and applying invalidates the whole plan rather than
/// half-renaming the workspace at offsets that no longer mean anything. That
/// is the only reason byte offsets from a language server are safe to apply.
fn apply_rename(workspace: &Workspace, plan: WorkspaceEditPlan) -> Result<Value, OperationError> {
    let mut by_file: BTreeMap<PathBuf, Vec<RangeReplacement>> = BTreeMap::new();
    for edit in &plan.edits {
        let path = crate::lsp::uri_path(&edit.uri);
        let relative = path
            .strip_prefix(workspace.path())
            .unwrap_or(&path)
            .to_path_buf();
        by_file.entry(relative).or_default().push(RangeReplacement {
            bytes: edit.bytes.clone(),
            replacement: edit.new_text.clone().into_bytes(),
        });
    }
    let on_disk = disk_revision(workspace, &by_file)?;

    let operations: Vec<EditOperation> = by_file
        .iter()
        .map(|(path, edits)| EditOperation {
            path: path.clone(),
            address: EditAddress::WorkspaceEdit {
                server: plan.server.clone(),
                revision: plan.revision,
                edits: edits.clone(),
            },
            replacement: Vec::new(),
        })
        .collect();

    // What the files are now. The addresses carry what they were when the plan
    // was made, and `EditAddress::WorkspaceEdit` refuses when the two differ.
    let resolver = PlanRevision {
        server: plan.server.clone(),
        revision: on_disk,
    };
    let applied = edit::apply_with_resolver(
        workspace.path(),
        &edit::EditTransaction {
            base: edit::workspace_version(workspace.path())
                .map_err(|error| OperationError::Execution(error.to_string()))?,
            operations,
        },
        &resolver,
    )
    .map_err(|error| OperationError::Execution(error.to_string()))?;

    Ok(serde_json::json!({
        "renamed": plan.new_name,
        "server": plan.server,
        "edits": plan.edits.len(),
        "files": applied
            .iter()
            .map(|file| serde_json::json!({
                "path": file.path.display().to_string(),
                "before": file.before.to_string(),
                "after": file.after.to_string(),
            }))
            .collect::<Vec<_>>(),
    }))
}

/// The plan's digest recomputed from the files as they are on disk now.
///
/// Keyed by URI rather than by path, because that is what the planner keyed it
/// by: the two sides have to hash the same names or every plan is stale.
fn disk_revision(
    workspace: &Workspace,
    by_file: &BTreeMap<PathBuf, Vec<RangeReplacement>>,
) -> Result<StateVersion, OperationError> {
    let mut documents = BTreeMap::new();
    for path in by_file.keys() {
        let content = workspace
            .read(path, crate::intelligence::MAX_DOCUMENT_BYTES)
            .map_err(|error| {
                OperationError::Execution(format!(
                    "the rename touches {}, which cannot be read: {error}",
                    path.display()
                ))
            })?;
        let text = String::from_utf8(content.bytes).map_err(|_| {
            OperationError::Execution(format!(
                "the rename touches {}, which is not UTF-8",
                path.display()
            ))
        })?;
        documents.insert(crate::lsp::file_uri(&workspace.path().join(path)), text);
    }
    Ok(crate::intelligence::document_revision(
        documents
            .iter()
            .map(|(uri, text)| (uri.as_str(), text.as_str())),
    ))
}

/// Confirms the files are what the plan was computed against.
struct PlanRevision {
    server: String,
    revision: StateVersion,
}

impl edit::SemanticResolver for PlanRevision {
    fn resolve_symbol(
        &self,
        _path: &Path,
        _symbol: &SymbolId,
    ) -> Result<edit::ResolvedSymbol, edit::EditError> {
        Err(edit::EditError::SemanticUnavailable)
    }

    fn server_revision(&self, server: &str) -> Result<StateVersion, edit::EditError> {
        if server == self.server {
            Ok(self.revision)
        } else {
            Err(edit::EditError::SemanticUnavailable)
        }
    }
}

/// No configured server claims this file's extension.
fn unserved(path: &Path) -> OperationError {
    OperationError::Execution(format!(
        "no language server is configured for {}; add one under `[lsp.server.<name>]` with the \
         extensions it answers for",
        path.display()
    ))
}

/// The tier that answered. `none` when there was nothing to answer with, which
/// is different from a tier that answered with nothing to report.
fn provider_of(provider: Option<crate::intelligence::EvidenceProvider>) -> String {
    provider.map_or_else(
        || "none".to_owned(),
        |provider| {
            serde_json::to_value(provider)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "none".to_owned())
        },
    )
}

fn symbol(raw: &str) -> Result<SymbolId, OperationError> {
    SymbolId::new(raw).map_err(|_| {
        OperationError::Execution(
            "`symbol` must be an id returned by code.symbol, such as `symbol:src/lib.rs#run`"
                .to_owned(),
        )
    })
}

fn semantic(error: IntelligenceError) -> OperationError {
    OperationError::Execution(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        artifact::{ArtifactReadLimits, FileArtifactStore},
        domain::{OperationId, Principal},
    };

    const LIMITS: ArtifactReadLimits = ArtifactReadLimits {
        max_bytes: 1024 * 1024,
        max_expansion_ratio: 1_000,
    };

    struct Fixture {
        _directory: tempfile::TempDir,
        workspace: Workspace,
        artifacts: Arc<dyn ArtifactStore>,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("src")).unwrap();
        std::fs::write(
            directory.path().join("src/engine.rs"),
            "/// Runs the thing.\npub fn run(times: u32) -> u32 {\n    times + 1\n}\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("src/main.rs"),
            "use crate::engine;\n\nfn main() {\n    // run is mentioned here\n    engine::run(1);\n}\n",
        )
        .unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(directory.path().join(".arsy/art"), 0).unwrap());
        Fixture {
            _directory: directory,
            workspace,
            artifacts,
        }
    }

    fn run(fixture: &Fixture, operation: CodeOperation, input: Value) -> Value {
        let executor = CodeExecutor::new(
            operation,
            &fixture.workspace,
            Arc::clone(&fixture.artifacts),
            0,
            Vec::new(),
        );
        let request = OperationRequest {
            id: OperationId::new(),
            kind: OperationKind::new(operation.kind()).unwrap(),
            actor: Principal::User("tester".into()),
            input,
            requirements: Vec::new(),
        };
        let outcome = executor.execute(&request, &[]).unwrap();
        let reference = outcome.value.expect("a semantic call stores its answer");
        let id: arsy_kernel::domain::ArtifactId = reference.value().parse().unwrap();
        let bytes = fixture.artifacts.read(id, LIMITS).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn a_declaration_is_found_read_and_traced_to_what_imports_it() {
        let fixture = fixture();

        let found = run(
            &fixture,
            CodeOperation::Symbol,
            serde_json::json!({"name": "run"}),
        );
        // The declaration, not the comment in main.rs that mentions it.
        assert_eq!(found["provider"], "syntax");
        assert_eq!(found["symbols"].as_array().unwrap().len(), 1);
        let symbol = found["symbols"][0].clone();
        assert_eq!(symbol["name"], "run");
        assert_eq!(symbol["location"]["uri"], "file:src/engine.rs");
        assert_eq!(symbol["id"], "symbol:src/engine.rs#run");

        let explained = run(
            &fixture,
            CodeOperation::Explain,
            serde_json::json!({"symbol": "symbol:src/engine.rs#run"}),
        );
        assert_eq!(explained["provider"], "syntax");
        let summary = explained["summary"].as_str().unwrap();
        assert!(summary.starts_with("function_item run"), "{summary}");
        assert!(summary.contains("times + 1"), "{summary}");

        let references = run(
            &fixture,
            CodeOperation::References,
            serde_json::json!({"symbol": "symbol:src/engine.rs#run"}),
        );
        let callers = references["callers"].as_array().unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0]["location"]["uri"], "file:src/main.rs");
        // An import is weaker evidence than a declaration, and says so.
        assert_eq!(callers[0]["confidence_basis_points"], 3_000);
    }

    #[test]
    fn a_name_no_grammar_knows_falls_back_to_text_rather_than_failing() {
        let fixture = fixture();
        std::fs::write(
            fixture.workspace.path().join("notes.md"),
            "the widget is documented here\n",
        )
        .unwrap();

        let found = run(
            &fixture,
            CodeOperation::Symbol,
            serde_json::json!({"name": "widget"}),
        );

        assert_eq!(found["provider"], "text");
        assert_eq!(found["symbols"][0]["location"]["uri"], "file:notes.md");
    }

    #[test]
    fn an_id_no_call_produced_is_refused_with_the_shape_that_works() {
        let fixture = fixture();
        let executor = CodeExecutor::new(
            CodeOperation::Explain,
            &fixture.workspace,
            Arc::clone(&fixture.artifacts),
            0,
            Vec::new(),
        );
        let request = OperationRequest {
            id: OperationId::new(),
            kind: OperationKind::new("code.explain").unwrap(),
            actor: Principal::System,
            input: serde_json::json!({"symbol": "run"}),
            requirements: Vec::new(),
        };

        let error = executor.execute(&request, &[]).unwrap_err();

        assert!(format!("{error}").contains("symbol:"), "{error}");
    }

    #[test]
    fn a_rename_is_applied_as_one_transaction_or_not_at_all() {
        let fixture = fixture();
        let root = fixture.workspace.path().to_owned();
        let engine = root.join("src/engine.rs");
        let main = root.join("src/main.rs");
        // A plan carries the digest of the text its offsets were computed
        // against, which is what a planner produces and what the applier
        // checks; building one by hand means computing it the same way.
        let plan = |edits: Vec<crate::intelligence::WorkspaceTextEdit>| {
            let documents: std::collections::BTreeMap<String, String> = edits
                .iter()
                .filter_map(|edit: &crate::intelligence::WorkspaceTextEdit| {
                    let text = std::fs::read_to_string(crate::lsp::uri_path(&edit.uri)).ok()?;
                    Some((edit.uri.clone(), text))
                })
                .collect();
            WorkspaceEditPlan {
                server: "fake".to_owned(),
                revision: crate::intelligence::document_revision(
                    documents
                        .iter()
                        .map(|(uri, text)| (uri.as_str(), text.as_str())),
                ),
                symbol: SymbolId::new("lsp:x#0:0").unwrap(),
                new_name: "walk".to_owned(),
                edits,
            }
        };
        let edit = |path: &std::path::Path, bytes: std::ops::Range<usize>| {
            crate::intelligence::WorkspaceTextEdit {
                uri: crate::lsp::file_uri(path),
                bytes,
                new_text: "walk".to_owned(),
            }
        };

        // `pub fn run` in engine.rs, `engine::run(1)` in main.rs.
        let engine_source = std::fs::read_to_string(&engine).unwrap();
        let main_source = std::fs::read_to_string(&main).unwrap();
        let in_engine = engine_source.find("run").unwrap();
        let in_main = main_source.rfind("run").unwrap();

        // One edit addresses a file that is not there: nothing is written.
        let missing = root.join("src/absent.rs");
        let refused = apply_rename(
            &fixture.workspace,
            plan(vec![
                edit(&engine, in_engine..in_engine + 3),
                edit(&missing, 0..1),
            ]),
        )
        .expect_err("an unreadable file invalidates the plan");
        assert!(format!("{refused}").contains("absent.rs"), "{refused}");
        assert_eq!(std::fs::read_to_string(&engine).unwrap(), engine_source);

        let applied = apply_rename(
            &fixture.workspace,
            plan(vec![
                edit(&engine, in_engine..in_engine + 3),
                edit(&main, in_main..in_main + 3),
            ]),
        )
        .expect("the plan applies");

        assert_eq!(applied["edits"], 2);
        assert_eq!(applied["files"].as_array().unwrap().len(), 2);
        assert!(std::fs::read_to_string(&engine)
            .unwrap()
            .contains("pub fn walk"));
        assert!(std::fs::read_to_string(&main)
            .unwrap()
            .contains("engine::walk"));
    }

    #[test]
    fn a_rename_planned_against_a_file_that_has_since_changed_is_refused() {
        let fixture = fixture();
        let engine = fixture.workspace.path().join("src/engine.rs");
        let source = std::fs::read_to_string(&engine).unwrap();
        let at = source.find("run").unwrap();
        let uri = crate::lsp::file_uri(&engine);
        // The revision a planner would have produced: a digest of the text the
        // offsets were computed against.
        let plan = WorkspaceEditPlan {
            server: "fake".to_owned(),
            revision: crate::intelligence::document_revision([(uri.as_str(), source.as_str())]),
            symbol: SymbolId::new("lsp:x#0:0").unwrap(),
            new_name: "walk".to_owned(),
            edits: vec![crate::intelligence::WorkspaceTextEdit {
                uri,
                bytes: at..at + 3,
                new_text: "walk".to_owned(),
            }],
        };

        // Someone else edits the file between planning and applying. The
        // offsets in the plan now point at the wrong bytes.
        let moved = format!("// a line nobody planned around\n{source}");
        std::fs::write(&engine, &moved).unwrap();

        let refused = apply_rename(&fixture.workspace, plan.clone())
            .expect_err("a plan computed against other text must not be applied");

        assert!(
            format!("{refused}").to_lowercase().contains("stale"),
            "{refused}"
        );
        assert_eq!(
            std::fs::read_to_string(&engine).unwrap(),
            moved,
            "a refused plan writes nothing"
        );

        // Put the file back, and the same plan applies: the check is about the
        // bytes, not about time having passed.
        std::fs::write(&engine, &source).unwrap();
        apply_rename(&fixture.workspace, plan)
            .expect("the plan applies to the text it was made for");
        assert!(std::fs::read_to_string(&engine)
            .unwrap()
            .contains("pub fn walk"));
    }

    #[test]
    fn a_file_no_configured_server_serves_is_refused_by_name() {
        let fixture = fixture();
        let executor = CodeExecutor::new(
            CodeOperation::Diagnostics,
            &fixture.workspace,
            Arc::clone(&fixture.artifacts),
            0,
            Vec::new(),
        );

        let error = executor
            .execute(
                &OperationRequest {
                    id: OperationId::new(),
                    kind: OperationKind::new("code.diagnostics").unwrap(),
                    actor: Principal::System,
                    input: serde_json::json!({"path": "src/engine.rs"}),
                    requirements: Vec::new(),
                },
                &[],
            )
            .unwrap_err();

        assert!(format!("{error}").contains("lsp.server"), "{error}");
    }
}
