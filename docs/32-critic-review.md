# Critical review and revision record

## Performance reviewer

**Attack:** event sourcing, policy mediation, artifacts, and semantic indexes can turn every action into serialization and I/O. **Revision:** in-process typed dispatch, batched SQLite append, lazy workers/indexes, bounded artifact previews, and explicit latency SLOs. RocksDB and microservices were removed from the baseline.

## Security reviewer

**Attack:** declarative hooks, LSP/DAP, MCP, formatters, and Git all execute repository-influenced code; a WASM label alone is not safety. **Revision:** every executor is capability-mediated; repository config cannot self-grant; adapters/servers run under policy; WASM receives explicit imports and quotas; sandbox failure closes; secret redaction occurs at every external sink.

## Rust reviewer

**Attack:** async traits with borrowed context, giant enums, and hundreds of crates would create lifetime, compile-time, and evolution pain. **Revision:** boxed futures only at stable dynamic boundaries, owned IDs/artifact references across tasks, three initial crates with logical modules, extraction gates, bounded task ownership, and pure event reducers.

## Agent researcher

**Attack:** too much type machinery can hide useful evidence and force weak models through complex schemas. **Revision:** model-facing schemas are generated/simplified per profile; semantic operations return compact cited evidence; the prompt compiler can fall back to conservative formats; every complexity change is tested across weaker and frontier models.

## UX reviewer

**Attack:** capability vocabulary, assurance levels, and compatibility loss reports could overwhelm users. **Revision:** frontends show intended effect, scope, reversibility, and reason; safe defaults hide machinery; `explain` views reveal full provenance on demand. Existing CLAUDE/AGENTS conventions work without native config.

## Compatibility reviewer

**Attack:** “canonical” translation can subtly break precedence, hooks, session ordering, or protocol quirks. **Revision:** adapters retain original artifacts, version mappings independently, publish loss diagnostics, and graduate behavior only through golden fixtures. Proprietary Claude internals are explicitly not claimed.

## Operations reviewer

**Attack:** daemon crashes, slow subscribers, worker leaks, schema migrations, and remote partitions can corrupt perceived state. **Revision:** immutable committed events, idempotency keys, resumable cursors, leases, backpressure/gap events, rebuildable projections, orphan cleanup, and backup-first explicit migrations.

## First-principles verdict

The design retains tool calls, Markdown instructions, worktrees, JSONL, URIs, MCP, and chat only where they are useful compatibility, presentation, export, or backend choices. Canonical primitives remain goals, agents, context views, operations, resources, effects, capabilities, workspaces, events, and evidence.

## Unresolved validation work

The major remaining risks are cross-platform sandbox equivalence, provider-profile drift, semantic-index freshness, compatibility fidelity for closed behavior, and whether multi-agent/knowledge-graph complexity improves success per token. Each has a roadmap gate; none is presented as solved.
