# Product requirements

## Users and jobs

Individual developers need a trustworthy local agent; teams need shared policy and auditable handoff; extension authors need stable, least-privilege APIs; IDEs need a durable session; platform operators need measurable safety and cost.

## Functional requirements

| Priority | Requirement |
|---|---|
| P0 | Canonical protocol, event store, provider abstraction, prompt compiler, policy evaluation, local execution, read/search/edit, approval, artifacts, recovery |
| P1 | Git-aware verification, syntax intelligence, LSP, portable sandbox profiles, Claude/Codex/OMP instruction imports, MCP client, CLI/TUI |
| P2 | DAP, knowledge graph, multi-agent isolation, WASM plugins, ACP/IDE, model routing, structured review |
| P3 | Remote workers, collaboration, MCP server/apps, advanced overlays, learned offline optimizers |

P0 establishes architecture; P1 makes a credible coding agent; P2 creates differentiation; P3 is optional until demand and eval evidence exist.

## Non-functional requirements

- Local/offline code operations; network only when provider or explicit capability requires it.
- Linux, macOS, and native Windows support with separately stated guarantees.
- Crash-consistent sessions; resumable streams and processes where the target supports them.
- Secrets redacted before model, log, telemetry, plugin, and external-protocol boundaries.
- Deterministic config resolution and explainable policy decisions.
- Version CLI, protocol, config, plugin ABI, session schema, and compatibility adapters independently.

## Acceptance criteria

1. Import fixtures prove deterministic AGENTS.md, CLAUDE.md, skills, hooks, and MCP translation.
2. A provider conformance suite adds a provider without changing capability implementations.
3. An edit rejected on stale state leaves no partial mutations.
4. Replaying canonical events reconstructs the same authoritative session state.
5. Parallel writer agents cannot mutate the same filesystem view.
6. Every external effect links to policy decision and verification evidence.
7. Lossless resume works after compaction and process crash.
8. Eval reports compare model × harness combinations and classify failures.

## Exclusions for the first credible release

Shared-control collaboration, Kubernetes workers, native in-process plugins, automated commit splitting, general database kernels, and self-modifying prompts are excluded. They enter only after a measured need.
