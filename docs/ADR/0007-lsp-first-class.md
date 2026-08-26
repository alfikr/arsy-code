# ADR-0007: LSP-backed semantic capability layer

- Status: Accepted
- Date: 2026-08-26

## Context

OMP verifies useful LSP operations (**V**), and LSP standardizes language services (**D**). Raw method exposure burdens models with protocol details.

## Decision

Host LSP as first-class infrastructure and expose task-oriented operations such as find symbol/callers, explain symbol, rename, and fix diagnostic. Tree-sitter and text remain fallbacks.

## Consequences

Semantic reliability and token efficiency should improve; server lifecycle, repository-executed initialization, document versions, and conflicting diagnostics add complexity.

## Alternatives

Text-only is universal but error-prone. Raw LSP tools are flexible but model-hostile. A custom language engine is infeasible.

## Invariant

Every semantic result is bound to workspace/document revision and provider provenance.
