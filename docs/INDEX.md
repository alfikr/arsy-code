# ARSY architecture specification

Status: implementation-ready design baseline, 2026-08-26. Runtime implementation is intentionally out of scope.

Start with [vision](00-vision.md), [research](01-competitive-research.md), [architecture](04-system-architecture.md), and [roadmap](31-roadmap.md). Evidence and pinned revisions live in [report-source.md](report-source.md). Where documents conflict, this numbered set and accepted records in `ADR/` govern.

| Area | Documents |
|---|---|
| Product | [00](00-vision.md), [02](02-product-requirements.md), [03](03-design-principles.md), [35](35-configuration.md) |
| Core runtime | [04](04-system-architecture.md), [05](05-rust-workspace.md), [06](06-agent-runtime.md), [07](07-context-engine.md), [08](08-prompt-compiler.md), [09](09-model-provider-layer.md), [10](10-tool-capability-system.md) |
| Code and execution | [11](11-code-intelligence.md), [12](12-edit-engine.md), [13](13-execution-runtime.md), [14](14-security-sandbox.md), [15](15-policy-permissions.md) |
| State and agents | [16](16-session-persistence.md), [17](17-memory-knowledge.md), [18](18-agent-orchestration.md), [19](19-plugin-extension-system.md) |
| Protocols and compatibility | [20](20-protocols.md), [21](21-compatibility-claude.md), [22](22-compatibility-codex.md), [23](23-compatibility-omp.md), [24](24-mcp-acp.md), [25](25-ide-integration.md) |
| Quality | [26](26-observability.md), [27](27-evaluation-benchmarks.md), [28](28-performance.md), [29](29-threat-model.md), [30](30-testing-strategy.md), [31](31-roadmap.md), [32](32-critic-review.md), [33](33-diagnostics.md), [34](34-distribution.md) |

## Evidence notation

- **V** — verified in source at a pinned revision.
- **D** — behavior documented by its publisher, but implementation not inspected.
- **I** — inference, explicitly bounded.
- **P** — ARSY proposal or target, not an achieved capability.

## Global invariants

1. A provider cannot mutate a workspace.
2. Every effect is authorized against canonical capabilities and a resolved resource.
3. Every persisted event has a stable ID, actor, causation, and schema version.
4. Compaction changes a context view, never canonical history.
5. Compatibility formats never become core domain types.
6. Plugins receive only declared, granted capabilities.
7. Every mutation is attributable to session, turn, agent, operation, and workspace revision.
8. Untrusted content cannot increase authority.
9. UI state is a projection, not agent truth.
10. Claims of completion require recorded verification evidence.
