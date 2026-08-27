# Implementation roadmap

## Sequencing

| Phase | Deliverable | Priority | Exit gate |
|---|---|---|---|
| 0 | evidence baseline, ADRs, fixture corpus | P0 | this specification accepted; unknowns assigned |
| 1 | domain/events, SQLite+CAS, embedded service/protocol, one provider, read/search/process/edit, policy, minimal eval | P0 | replay/crash/edit/policy properties pass; first tasks reproducible |
| 2 | prompt/context compiler, provider profiles, Git verification, syntax/tree-sitter, artifacts/telemetry | P1 | multi-model conformance and success/token baseline |
| 3 | Linux/macOS/Windows sandbox workers, secrets, approval UI | P1 | platform attack suites and no-silent-degradation tests pass |
| 4 | LSP semantic API and transactional semantic edits | P1/P2 | reference languages beat text-only baseline |
| 5 | Claude/Codex/OMP instruction, skill, hook, provider, and MCP imports | P1 | golden compatibility fixtures and loss reports pass |
| 6 | DAP, structured review, risk-based verification | P2 | debugger tasks beat print-debug baseline |
| 7 | durable agent graph, writer isolation, observers | P2 | parallel tasks show net gain without corruption |
| 8 | WASM plugins, MCP server/apps, ACP and reference IDE | P2/P3 | permission/conformance suites pass |
| 9 | memory/knowledge graph, routing, remote targets, collaboration | P3 | each feature separately improves held-out evals |

## Dependency logic

Persistence, protocol, policy, artifacts, and evals precede autonomy. Sandboxing precedes broad extension execution. Code intelligence precedes sophisticated multi-agent work because better evidence often removes the need for more agents. Compatibility import precedes protocol emulation. Routing and memory are late because they amplify both good and bad behavior.

## Migration and versioning

Version CLI releases with SemVer; protocol, config, plugin API, session schema, and each compatibility adapter carry independent versions/capabilities. Migrations are explicit commands with backup, dry-run, loss report, and rollback guidance; [`arsy migrate`](36-cli-tui.md) reports without writing unless `--apply` is given. Old sessions are never silently rewritten.

## First implementation slice

One local Git workspace, embedded CLI/service, SQLite/CAS, one model adapter, immutable events, context compilation, typed file/search/process/edit operations, user/workspace policy, content-hash stale-write rejection, and targeted test evidence. No daemon default, LSP, WASM, remote workers, or subagents until this slice is reliable.
