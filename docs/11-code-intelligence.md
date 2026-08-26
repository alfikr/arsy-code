# Code intelligence

## Problem and existing approaches

Text-only agents repeatedly search and guess semantic relationships. OMP verifies practical LSP and DAP integration (**V**). LSP standardizes language services (**D**), but raw protocol calls expose too much incidental complexity to a model.

## Intelligence ladder

| Level | Source | Use |
|---:|---|---|
| 0 | bytes/text | exact reads and hashes |
| 1 | ignore-aware lexical search | names, literals, broad discovery |
| 2 | tree-sitter syntax | structure, nodes, imports, cheap edits |
| 3 | LSP symbols/types/references | semantic identity and diagnostics |
| 4 | dependency/build graph | cross-package impact |
| 5 | Git history | ownership, intent, regression evidence |
| 6 | DAP/runtime traces | actual behavior |

The planner chooses the cheapest sufficient level and escalates when evidence is ambiguous.

## Semantic API

```rust
pub trait CodeIntelligence: Send + Sync {
    fn find_symbol(&self, q: SymbolQuery) -> BoxFuture<'_, Result<Vec<SymbolHit>>>;
    fn explain_symbol(&self, id: SymbolId) -> BoxFuture<'_, Result<SymbolEvidence>>;
    fn find_callers(&self, id: SymbolId) -> BoxFuture<'_, Result<ReferenceGraph>>;
    fn diagnostics(&self, scope: CodeScope) -> BoxFuture<'_, Result<DiagnosticSet>>;
    fn plan_rename(&self, id: SymbolId, name: String)
        -> BoxFuture<'_, Result<WorkspaceEditPlan>>;
}
```

Raw hover/definition/references/implementations/symbols/code-actions/formatting/call hierarchy/type hierarchy remain adapter internals. Results include source revision, provider (syntax/LSP/build), confidence, and artifacts.

## Incremental knowledge graph

Nodes are files, modules, symbols, types, tests, packages, services, endpoints, configs, and data objects. Edges include imports, calls, implements, references, tests, depends-on, configures, and owns. File watching and Git diff invalidate syntax nodes; LSP/build results enrich them. Unknown or stale edges are explicit. No full re-index occurs on every startup.

## Failure, security, performance

Server crashes restart with bounded backoff; unsaved overlays are versioned; conflicting servers do not silently merge facts. Repository-controlled server commands require policy because opening a project can execute code. Index by content hash, batch LSP requests, cap graph fan-out, and return compact evidence.

## Decision and open questions

LSP is first-class infrastructure behind higher operations; tree-sitter provides offline fallback. Language-by-language confidence calibration and build graph adapters are eval-driven.
