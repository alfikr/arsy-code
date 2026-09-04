//! Compact task-oriented code intelligence over an LSP transport.

use crate::{
    lsp::{LspError, LspHost, LspRequest, LspTransport, MAX_LSP_BATCH},
    resource::Workspace,
};
use arsy_kernel::domain::StateVersion;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fmt, ops::Range};

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
