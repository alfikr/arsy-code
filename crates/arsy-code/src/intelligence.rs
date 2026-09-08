//! Compact task-oriented code intelligence over an LSP transport.

use crate::{
    lsp::{LspError, LspHost, LspRequest, LspTransport, MAX_LSP_BATCH},
    resource::Workspace,
    syntax::RustSyntax,
};
use arsy_kernel::domain::StateVersion;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    ops::Range,
    path::{Path, PathBuf},
};

pub const MAX_SEMANTIC_RESULTS: usize = 256;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceProvider {
    Lsp,
    Syntax,
    Text,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SymbolId(String);

impl SymbolId {
    pub fn new(value: impl Into<String>) -> Result<Self, IntelligenceError> {
        let value = value.into();
        if value.is_empty() || value.len() > 1024 {
            return Err(IntelligenceError::InvalidSymbol);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolQuery {
    pub name: String,
    pub max_results: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceLocation {
    pub uri: String,
    pub bytes: Range<usize>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SymbolHit {
    pub id: SymbolId,
    pub name: String,
    pub location: SourceLocation,
    pub source_revision: StateVersion,
    pub provider: EvidenceProvider,
    pub confidence_basis_points: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SymbolEvidence {
    pub symbol: SymbolId,
    pub summary: String,
    pub citations: Vec<SourceLocation>,
    pub source_revision: StateVersion,
    pub provider: EvidenceProvider,
    pub confidence_basis_points: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReferenceGraph {
    pub callers: Vec<SymbolHit>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CodeDiagnostic {
    pub location: SourceLocation,
    pub severity: u8,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiagnosticSet {
    pub diagnostics: Vec<CodeDiagnostic>,
    pub source_revision: StateVersion,
    pub provider: EvidenceProvider,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceTextEdit {
    pub uri: String,
    pub bytes: Range<usize>,
    pub new_text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceEditPlan {
    pub server: String,
    pub revision: StateVersion,
    pub symbol: SymbolId,
    pub new_name: String,
    pub edits: Vec<WorkspaceTextEdit>,
}

pub trait CodeIntelligence {
    fn find_symbol(&mut self, query: &SymbolQuery) -> Result<Vec<SymbolHit>, IntelligenceError>;
    fn explain_symbol(&mut self, id: &SymbolId) -> Result<SymbolEvidence, IntelligenceError>;
    fn find_callers(&mut self, id: &SymbolId) -> Result<ReferenceGraph, IntelligenceError>;
    fn diagnostics(&mut self, scope: &str) -> Result<DiagnosticSet, IntelligenceError>;
    fn plan_rename(
        &mut self,
        id: &SymbolId,
        name: &str,
    ) -> Result<WorkspaceEditPlan, IntelligenceError>;
}

pub struct LspCodeIntelligence<T> {
    server: String,
    revision: StateVersion,
    host: LspHost<T>,
}

impl<T: LspTransport> LspCodeIntelligence<T> {
    pub fn new(server: impl Into<String>, revision: StateVersion, host: LspHost<T>) -> Self {
        Self {
            server: server.into(),
            revision,
            host,
        }
    }

    fn one(&mut self, method: &str, params: Value) -> Result<Value, IntelligenceError> {
        self.host
            .request_batch(vec![LspRequest {
                method: method.into(),
                params,
            }])?
            .pop()
            .ok_or_else(|| IntelligenceError::Protocol("missing LSP result".into()))
    }

    pub fn request_many(
        &mut self,
        requests: Vec<LspRequest>,
    ) -> Result<Vec<Value>, IntelligenceError> {
        if requests.len() > MAX_LSP_BATCH {
            return Err(IntelligenceError::FanoutExceeded);
        }
        self.host.request_batch(requests).map_err(Into::into)
    }
}

#[derive(Deserialize)]
struct RawSymbol {
    name: String,
    uri: String,
    start: usize,
    end: usize,
}

impl<T: LspTransport> CodeIntelligence for LspCodeIntelligence<T> {
    fn find_symbol(&mut self, query: &SymbolQuery) -> Result<Vec<SymbolHit>, IntelligenceError> {
        if query.name.is_empty() || query.max_results == 0 {
            return Err(IntelligenceError::InvalidQuery);
        }
        let limit = query.max_results.min(MAX_SEMANTIC_RESULTS);
        let raw: Vec<RawSymbol> = serde_json::from_value(self.one(
            "workspace/symbol",
            json!({"query": query.name, "limit": limit}),
        )?)
        .map_err(|error| IntelligenceError::Protocol(error.to_string()))?;
        raw.into_iter()
            .take(limit)
            .map(|raw| hit(raw, self.revision, EvidenceProvider::Lsp, 9_000))
            .collect()
    }

    fn explain_symbol(&mut self, id: &SymbolId) -> Result<SymbolEvidence, IntelligenceError> {
        #[derive(Deserialize)]
        struct RawEvidence {
            summary: String,
            citations: Vec<SourceLocation>,
        }
        let raw: RawEvidence =
            serde_json::from_value(self.one("arsy/explainSymbol", json!({"symbol": id.as_str()}))?)
                .map_err(|error| IntelligenceError::Protocol(error.to_string()))?;
        if raw.summary.len() > 16 * 1024 || raw.citations.len() > MAX_SEMANTIC_RESULTS {
            return Err(IntelligenceError::ResultTooLarge);
        }
        Ok(SymbolEvidence {
            symbol: id.clone(),
            summary: raw.summary,
            citations: raw.citations,
            source_revision: self.revision,
            provider: EvidenceProvider::Lsp,
            confidence_basis_points: 9_000,
        })
    }

    fn find_callers(&mut self, id: &SymbolId) -> Result<ReferenceGraph, IntelligenceError> {
        let raw: Vec<RawSymbol> = serde_json::from_value(self.one(
            "callHierarchy/incomingCalls",
            json!({"symbol": id.as_str()}),
        )?)
        .map_err(|error| IntelligenceError::Protocol(error.to_string()))?;
        let truncated = raw.len() > MAX_SEMANTIC_RESULTS;
        let callers = raw
            .into_iter()
            .take(MAX_SEMANTIC_RESULTS)
            .map(|raw| hit(raw, self.revision, EvidenceProvider::Lsp, 9_000))
            .collect::<Result<_, _>>()?;
        Ok(ReferenceGraph { callers, truncated })
    }

    fn diagnostics(&mut self, scope: &str) -> Result<DiagnosticSet, IntelligenceError> {
        let mut diagnostics: Vec<CodeDiagnostic> =
            serde_json::from_value(self.one("arsy/diagnostics", json!({"scope": scope}))?)
                .map_err(|error| IntelligenceError::Protocol(error.to_string()))?;
        let truncated = diagnostics.len() > MAX_SEMANTIC_RESULTS;
        diagnostics.truncate(MAX_SEMANTIC_RESULTS);
        Ok(DiagnosticSet {
            diagnostics,
            source_revision: self.revision,
            provider: EvidenceProvider::Lsp,
            truncated,
        })
    }

    fn plan_rename(
        &mut self,
        id: &SymbolId,
        name: &str,
    ) -> Result<WorkspaceEditPlan, IntelligenceError> {
        if name.is_empty() {
            return Err(IntelligenceError::InvalidQuery);
        }
        let mut edits: Vec<WorkspaceTextEdit> = serde_json::from_value(self.one(
            "textDocument/rename",
            json!({"symbol": id.as_str(), "newName": name}),
        )?)
        .map_err(|error| IntelligenceError::Protocol(error.to_string()))?;
        if edits.len() > MAX_SEMANTIC_RESULTS {
            return Err(IntelligenceError::ResultTooLarge);
        }
        edits.sort_by(|left, right| {
            left.uri
                .cmp(&right.uri)
                .then_with(|| left.bytes.start.cmp(&right.bytes.start))
        });
        Ok(WorkspaceEditPlan {
            server: self.server.clone(),
            revision: self.revision,
            symbol: id.clone(),
            new_name: name.into(),
            edits,
        })
    }
}

/// Level-2 fallback: tree-sitter declarations, indexed into the repository
/// graph.
///
/// Between the text tier, which can only find a string, and a language server,
/// which can prove what a name binds to. It knows a `fn foo` from a comment
/// mentioning `foo`, and it knows which files import the one a symbol lives
/// in — but an import is not a call and a declaration is not a definition site
/// proof, so it says so in its confidence and refuses the operations that
/// would need more than it has.
pub struct GraphCodeIntelligence<'a> {
    workspace: &'a Workspace,
    graph: crate::graph::KnowledgeGraph,
}

/// A symbol the graph tier found, addressed the way the graph addresses it.
///
/// The LSP tier passes an opaque server id through; this tier's ids are
/// `symbol:<path>#<name>`, so a follow-up call can find the declaration again
/// without the caller holding the graph.
struct GraphSymbol {
    path: PathBuf,
    name: String,
}

impl GraphSymbol {
    fn parse(id: &SymbolId) -> Result<Self, IntelligenceError> {
        let (path, name) = id
            .as_str()
            .strip_prefix("symbol:")
            .and_then(|rest| rest.rsplit_once('#'))
            .ok_or_else(|| {
                // Named rather than rejected as malformed: the caller almost
                // always passed the symbol's *name*, and the fix is to look it
                // up first rather than to spell the id differently.
                IntelligenceError::Protocol(format!(
                    "`{}` is not a symbol id; find one with code.symbol first, such as \
                     `symbol:src/lib.rs#run`",
                    id.as_str()
                ))
            })?;
        Ok(Self {
            path: PathBuf::from(path),
            name: name.to_owned(),
        })
    }
}

/// How much of a declaration `explain_symbol` quotes back.
const MAX_DECLARATION_BYTES: usize = 4 * 1024;

impl<'a> GraphCodeIntelligence<'a> {
    /// Index the workspace now. The graph itself is incremental, but a fresh
    /// process has nothing to be incremental against.
    pub fn index(workspace: &'a Workspace) -> Result<Self, IntelligenceError> {
        let mut graph = crate::graph::KnowledgeGraph::new();
        graph
            .index(workspace)
            .map_err(|error| IntelligenceError::Protocol(error.to_string()))?;
        Ok(Self { workspace, graph })
    }

    /// The declaration's byte range, found by re-parsing the file the graph
    /// says it is in. The graph stores identity, not offsets: an offset goes
    /// stale on the next edit, and the parse that would refresh it is the same
    /// parse that answers this question.
    fn locate(
        &self,
        path: &Path,
        name: &str,
    ) -> Result<(SourceLocation, StateVersion), IntelligenceError> {
        let content = self.workspace.read(path, crate::graph::MAX_INDEXED_BYTES)?;
        let syntax = RustSyntax::new(content.bytes.clone())
            .map_err(|error| IntelligenceError::Protocol(error.to_string()))?;
        for kind in crate::graph::DECLARATIONS {
            let Ok(declarations) = syntax.declarations(kind) else {
                continue;
            };
            if let Some((node, _)) = declarations.into_iter().find(|(_, found)| found == name) {
                return Ok((
                    SourceLocation {
                        uri: format!("file:{}", slash(path)),
                        bytes: node.bytes,
                    },
                    content.digest,
                ));
            }
        }
        Err(IntelligenceError::Protocol(format!(
            "{name} is indexed in {} but no longer declared there",
            path.display()
        )))
    }
}

impl CodeIntelligence for GraphCodeIntelligence<'_> {
    fn find_symbol(&mut self, query: &SymbolQuery) -> Result<Vec<SymbolHit>, IntelligenceError> {
        if query.name.is_empty() || query.max_results == 0 {
            return Err(IntelligenceError::InvalidQuery);
        }
        let found: Vec<(PathBuf, String)> = self
            .graph
            .symbols(&query.name)
            .into_iter()
            .filter_map(|node| Some((node.path.clone()?, node.name.clone())))
            .take(query.max_results.min(MAX_SEMANTIC_RESULTS))
            .collect();
        found
            .into_iter()
            .map(|(path, name)| {
                let (location, revision) = self.locate(&path, &name)?;
                Ok(SymbolHit {
                    id: SymbolId::new(crate::graph::NodeId::symbol(&path, &name).to_string())?,
                    name,
                    location,
                    source_revision: revision,
                    provider: EvidenceProvider::Syntax,
                    // A grammar proves this is a declaration of that name; it
                    // does not prove it is the one the caller meant.
                    confidence_basis_points: 6_000,
                })
            })
            .collect()
    }

    fn explain_symbol(&mut self, id: &SymbolId) -> Result<SymbolEvidence, IntelligenceError> {
        let symbol = GraphSymbol::parse(id)?;
        let node = self
            .graph
            .node(&crate::graph::NodeId::symbol(&symbol.path, &symbol.name))
            .ok_or(IntelligenceError::InvalidSymbol)?;
        let declaration = node.declaration.clone().unwrap_or_default();
        let (location, revision) = self.locate(&symbol.path, &symbol.name)?;
        let content = self
            .workspace
            .read(&symbol.path, crate::graph::MAX_INDEXED_BYTES)?;
        let text = content
            .bytes
            .get(location.bytes.clone())
            .map(|slice| String::from_utf8_lossy(slice).into_owned())
            .unwrap_or_default();
        let mut summary = format!("{declaration} {}\n", symbol.name);
        summary.push_str(&text[..text.len().min(MAX_DECLARATION_BYTES)]);
        Ok(SymbolEvidence {
            symbol: id.clone(),
            summary,
            citations: vec![location],
            source_revision: revision,
            provider: EvidenceProvider::Syntax,
            confidence_basis_points: 6_000,
        })
    }

    /// The files that import this symbol's module.
    ///
    /// Not callers: an import proves a file can reach the symbol, not that it
    /// uses it. Reported at low confidence rather than withheld, because "these
    /// eight files could be affected" is the answer a change needs and the text
    /// tier cannot give it at all.
    fn find_callers(&mut self, id: &SymbolId) -> Result<ReferenceGraph, IntelligenceError> {
        let symbol = GraphSymbol::parse(id)?;
        let importers: Vec<(PathBuf, String)> = self
            .graph
            .importers_of(&symbol.path)
            .into_iter()
            .filter_map(|node| Some((node.path.clone()?, node.name.clone())))
            .collect();
        let truncated = importers.len() > MAX_SEMANTIC_RESULTS;
        let callers = importers
            .into_iter()
            .take(MAX_SEMANTIC_RESULTS)
            .map(|(path, name)| {
                let content = self
                    .workspace
                    .read(&path, crate::graph::MAX_INDEXED_BYTES)?;
                Ok(SymbolHit {
                    id: SymbolId::new(crate::graph::NodeId::file(&path).to_string())?,
                    name,
                    location: SourceLocation {
                        uri: format!("file:{}", slash(&path)),
                        bytes: 0..0,
                    },
                    source_revision: content.digest,
                    provider: EvidenceProvider::Syntax,
                    confidence_basis_points: 3_000,
                })
            })
            .collect::<Result<_, IntelligenceError>>()?;
        Ok(ReferenceGraph { callers, truncated })
    }

    fn diagnostics(&mut self, _scope: &str) -> Result<DiagnosticSet, IntelligenceError> {
        Err(IntelligenceError::Unsupported("diagnostics"))
    }

    /// Refused rather than approximated.
    ///
    /// A grammar can find every declaration of a name; it cannot tell which
    /// uses of that name bind to this declaration, and a rename that is wrong
    /// about that silently breaks the build somewhere the caller is not
    /// looking. A language server answers this question or nobody does.
    fn plan_rename(
        &mut self,
        _id: &SymbolId,
        _name: &str,
    ) -> Result<WorkspaceEditPlan, IntelligenceError> {
        Err(IntelligenceError::Unsupported("plan_rename"))
    }
}

fn slash(path: &Path) -> String {
    path.components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// Level-1 fallback used when no language server is configured.
pub struct TextCodeIntelligence<'a> {
    workspace: &'a Workspace,
}

impl<'a> TextCodeIntelligence<'a> {
    pub const fn new(workspace: &'a Workspace) -> Self {
        Self { workspace }
    }
}

impl CodeIntelligence for TextCodeIntelligence<'_> {
    fn find_symbol(&mut self, query: &SymbolQuery) -> Result<Vec<SymbolHit>, IntelligenceError> {
        if query.name.is_empty() || query.max_results == 0 {
            return Err(IntelligenceError::InvalidQuery);
        }
        self.workspace
            .search(&query.name, query.max_results, 100_000, 16 * 1024 * 1024)?
            .hits
            .into_iter()
            .map(|found| {
                let path = found.resource.value();
                let content = self.workspace.resolve_file(path)?.read(16 * 1024 * 1024)?;
                let start = found.text.find(&query.name).unwrap_or_default();
                let raw = RawSymbol {
                    name: query.name.clone(),
                    uri: format!("file:{path}"),
                    start,
                    end: start + query.name.len(),
                };
                hit(raw, content.digest, EvidenceProvider::Text, 4_000)
            })
            .collect()
    }

    fn explain_symbol(&mut self, _id: &SymbolId) -> Result<SymbolEvidence, IntelligenceError> {
        Err(IntelligenceError::Unsupported("explain_symbol"))
    }

    fn find_callers(&mut self, _id: &SymbolId) -> Result<ReferenceGraph, IntelligenceError> {
        Err(IntelligenceError::Unsupported("find_callers"))
    }

    fn diagnostics(&mut self, _scope: &str) -> Result<DiagnosticSet, IntelligenceError> {
        Err(IntelligenceError::Unsupported("diagnostics"))
    }

    fn plan_rename(
        &mut self,
        _id: &SymbolId,
        _name: &str,
    ) -> Result<WorkspaceEditPlan, IntelligenceError> {
        Err(IntelligenceError::Unsupported("plan_rename"))
    }
}

fn hit(
    raw: RawSymbol,
    revision: StateVersion,
    provider: EvidenceProvider,
    confidence: u16,
) -> Result<SymbolHit, IntelligenceError> {
    if raw.name.is_empty() || raw.uri.is_empty() || raw.start > raw.end {
        return Err(IntelligenceError::Protocol(
            "invalid symbol location".into(),
        ));
    }
    let identity =
        Sha256::digest(format!("{}\0{}\0{}\0{}", raw.name, raw.uri, raw.start, raw.end).as_bytes());
    Ok(SymbolHit {
        id: SymbolId::new(StateVersion::from_digest(identity.into()).to_string())?,
        name: raw.name,
        location: SourceLocation {
            uri: raw.uri,
            bytes: raw.start..raw.end,
        },
        source_revision: revision,
        provider,
        confidence_basis_points: confidence,
    })
}

#[derive(Debug)]
pub enum IntelligenceError {
    InvalidQuery,
    InvalidSymbol,
    FanoutExceeded,
    ResultTooLarge,
    Unsupported(&'static str),
    Protocol(String),
    Lsp(LspError),
    Search(crate::search::SearchError),
    Resolve(crate::resource::ResolveError),
    Io(std::io::Error),
}

impl fmt::Display for IntelligenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQuery => formatter.write_str("semantic query is empty or unbounded"),
            Self::InvalidSymbol => formatter.write_str("symbol id is empty or too large"),
            Self::FanoutExceeded => formatter.write_str("semantic request fan-out exceeds its cap"),
            Self::ResultTooLarge => formatter.write_str("semantic result exceeds its cap"),
            Self::Unsupported(operation) => write!(formatter, "{operation} needs an LSP provider"),
            Self::Protocol(error) => write!(formatter, "semantic protocol error: {error}"),
            Self::Lsp(error) => error.fmt(formatter),
            Self::Search(error) => error.fmt(formatter),
            Self::Resolve(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for IntelligenceError {}

impl From<LspError> for IntelligenceError {
    fn from(value: LspError) -> Self {
        Self::Lsp(value)
    }
}

impl From<crate::search::SearchError> for IntelligenceError {
    fn from(value: crate::search::SearchError) -> Self {
        Self::Search(value)
    }
}

impl From<crate::resource::ResolveError> for IntelligenceError {
    fn from(value: crate::resource::ResolveError) -> Self {
        Self::Resolve(value)
    }
}

impl From<std::io::Error> for IntelligenceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{CommandOrigin, RestartPolicy, ServerCommand};

    struct Fake(Vec<Value>);

    impl LspTransport for Fake {
        fn start(&mut self, _command: &ServerCommand) -> Result<(), LspError> {
            Ok(())
        }

        fn stop(&mut self) {}

        fn request_batch(&mut self, requests: &[LspRequest]) -> Result<Vec<Value>, LspError> {
            assert!(requests.len() <= MAX_LSP_BATCH);
            Ok(self.0.drain(..requests.len()).collect())
        }
    }

    fn client(results: Vec<Value>) -> LspCodeIntelligence<Fake> {
        let host = LspHost::new(
            ServerCommand {
                argv: vec!["fake".into()],
                origin: CommandOrigin::Installed,
                policy_authorized: false,
            },
            Fake(results),
            RestartPolicy { delays: vec![] },
        );
        LspCodeIntelligence::new("rust-analyzer", StateVersion::from_digest([7; 32]), host)
    }

    #[test]
    fn semantic_results_are_compact_cited_versioned_and_bounded() {
        let mut client = client(vec![json!([
            {"name":"Thing", "uri":"file:///repo/a.rs", "start":4, "end":9}
        ])]);
        let hits = client
            .find_symbol(&SymbolQuery {
                name: "Thing".into(),
                max_results: 1,
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provider, EvidenceProvider::Lsp);
        assert_eq!(hits[0].source_revision, StateVersion::from_digest([7; 32]));
        assert_eq!(hits[0].location.bytes, 4..9);
    }

    #[test]
    fn rename_plan_is_revision_bound_and_sorted() {
        let mut client = client(vec![json!([
            {"uri":"file:///repo/b.rs", "bytes":{"start":9,"end":14}, "new_text":"New"},
            {"uri":"file:///repo/a.rs", "bytes":{"start":1,"end":6}, "new_text":"New"}
        ])]);
        let plan = client
            .plan_rename(&SymbolId::new("thing").unwrap(), "New")
            .unwrap();

        assert_eq!(plan.server, "rust-analyzer");
        assert_eq!(plan.edits[0].uri, "file:///repo/a.rs");
        assert_eq!(plan.revision, StateVersion::from_digest([7; 32]));
    }
}
