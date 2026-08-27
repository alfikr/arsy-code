# Testing strategy

## Test pyramid

| Layer | Focus |
|---|---|
| unit | reducers, precedence, scoring, parsers, capability intersections |
| property | event replay, edit atomicity, path normalization, policy monotonicity |
| snapshot/golden | prompts, protocol schemas, compatibility imports, UI projections |
| integration | SQLite/CAS, LSP/DAP adapters, shell/PTY, provider streams, hooks |
| sandbox | escape attempts, symlinks, network/filesystem/process ceilings per OS |
| protocol conformance | canonical, MCP, ACP, Codex façade and migration versions |
| chaos | crash points, truncation, worker loss, duplicate events, network partitions |
| end-to-end/eval | real repositories and hidden verification |

Fuzz protocol/config/patch parsers, permission patterns, model tool inputs and streaming deltas, artifact decoders, URI/path handling, and DAP/LSP framing. Seed corpora include valid ecosystem fixtures and minimized historical failures.

## Critical properties

1. Deny rules are monotonic: adding a deny cannot expand authority.
2. Child grants are subsets of parent grants.
3. Event replay is deterministic.
4. Failed edit transactions leave the base unchanged.
5. Compaction preserves reachability of covered events.
6. Redaction is idempotent and secrets never cross marked sinks.
7. Projection rebuild equals incremental projection.
8. Schema migrations either commit fully or preserve the old store.
9. Reconnect and refresh cannot widen authority: a reconnected connection or reloaded extension never gains a capability its previous grant did not include.

## CI and platform matrix

Linux, macOS, and Windows run native process/sandbox tests; no WSL substitution. Fast deterministic tests gate every change, platform integration runs on merge, fuzzers and end-to-end evals run continuously/nightly. Flaky tests are quarantined with ownership and deadline, never silently retried into green.

## Decision

Protocol, persistence, policy, edit, and sandbox tests precede feature breadth. Every production failure becomes a minimized regression fixture and taxonomy entry.
