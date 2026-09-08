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
    intelligence::{
        CodeIntelligence, GraphCodeIntelligence, IntelligenceError, SymbolId, SymbolQuery,
        TextCodeIntelligence, MAX_SEMANTIC_RESULTS,
    },
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
use serde::Serialize;
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

const DEFAULT_LIMIT: u64 = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodeOperation {
    /// Where a name is declared.
    Symbol,
    /// What one declaration is.
    Explain,
    /// What could be affected by changing it.
    References,
}

impl CodeOperation {
    pub const ALL: [Self; 3] = [Self::Symbol, Self::Explain, Self::References];

    const fn kind(self) -> &'static str {
        match self {
            Self::Symbol => "code.symbol",
            Self::Explain => "code.explain",
            Self::References => "code.references",
        }
    }

    fn schema(self) -> InputSchema {
        match self {
            Self::Symbol => InputSchema {
                required: BTreeMap::from([("name".to_owned(), JsonType::String)]),
                optional: BTreeMap::from([("limit".to_owned(), JsonType::Number)]),
                allow_extra: false,
            },
            Self::Explain | Self::References => InputSchema {
                required: BTreeMap::from([("symbol".to_owned(), JsonType::String)]),
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
}

impl CodeExecutor {
    pub fn new(
        operation: CodeOperation,
        workspace: &Workspace,
        artifacts: Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            operation,
            contract: OperationContract {
                kind: OperationKind::new(operation.kind()).expect("static operation kind is valid"),
                input_schema: operation.schema(),
                // Reading the repository, however cleverly. A semantic answer
                // needs no authority a file read does not.
                actions: vec![CapabilityAction::FsRead],
                idempotency: Idempotency::Idempotent,
                reversible: true,
                concurrency: ConcurrencyRule::Parallel,
            },
            workspace: workspace.path().to_owned(),
            artifacts,
            retain_until_ms,
        })
    }
}

pub fn executors(
    workspace: &Workspace,
    artifacts: &Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
) -> Vec<Arc<dyn OperationExecutor>> {
    CodeOperation::ALL
        .into_iter()
        .map(|operation| {
            CodeExecutor::new(operation, workspace, Arc::clone(artifacts), retain_until_ms)
                as Arc<dyn OperationExecutor>
        })
        .collect()
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
        let text = |key: &str| {
            request
                .input
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };

        // Indexing is per call rather than cached because an operation cannot
        // know what the last one edited; the graph is incremental against a
        // store this layer does not have, and re-parsing a repository is
        // cheaper than answering from a stale index.
        let mut graph = GraphCodeIntelligence::index(&workspace).map_err(semantic)?;
        let mut fallback = TextCodeIntelligence::new(&workspace);

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
                // A workspace with no indexed declaration of that name is not
                // an error: the string may still be there, and finding it is
                // the text tier's job.
                let hits = match graph.find_symbol(&query) {
                    Ok(hits) if !hits.is_empty() => hits,
                    Ok(_) | Err(IntelligenceError::Unsupported(_)) => {
                        fallback.find_symbol(&query).map_err(semantic)?
                    }
                    Err(error) => return Err(semantic(error)),
                };
                CodeOutcome {
                    provider: provider_of(hits.first().map(|hit| hit.provider)),
                    answer: serde_json::json!({"symbols": hits}),
                }
            }
            CodeOperation::Explain => {
                let evidence = graph
                    .explain_symbol(&symbol(&text("symbol"))?)
                    .map_err(semantic)?;
                CodeOutcome {
                    provider: provider_of(Some(evidence.provider)),
                    answer: serde_json::to_value(evidence)
                        .map_err(|error| OperationError::Execution(error.to_string()))?,
                }
            }
            CodeOperation::References => {
                let graph_result = graph
                    .find_callers(&symbol(&text("symbol"))?)
                    .map_err(semantic)?;
                CodeOutcome {
                    provider: provider_of(graph_result.callers.first().map(|hit| hit.provider)),
                    answer: serde_json::to_value(graph_result)
                        .map_err(|error| OperationError::Execution(error.to_string()))?,
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
                action: CapabilityAction::FsRead,
                resource: ResourceRef::new("workspace", "*").expect("a static scheme and value"),
            }],
            evidence: Vec::new(),
            state: None,
        })
    }
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
}
