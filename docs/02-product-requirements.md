# Product requirements

## Users and jobs

Individual developers need a trustworthy local agent; teams need shared policy and auditable handoff; extension authors need stable, least-privilege APIs; IDEs need a durable session; platform operators need measurable safety and cost.

## Functional requirements

| Priority | Requirement |
|---|---|
| P0 | Preserve the canonical operation/policy/capability path; make task attempts, budgets, validation, and recovery durable; ship asynchronous agents, isolated writers, completion proofs, and isolated regression benchmarks |
| P1 | Agent Hub, Safe Auto, responsibility-based roles, measured model routing, plan-to-commit compilation, and verified launch readiness |
| P2 | Benchmark-driven language semantics, structural intelligence, debugger repair loops, and automatic memory curation |
| P3 | Remote/distributed workers, organization collaboration, daemon scheduling, advanced overlays, marketplace, and learned offline optimizers |

The event, capability, provider, tool, and evaluation foundations already have
implemented slices. These priorities name the remaining delivery order, not the
historical order in which source files appeared. See the source-backed status
and gates in [`31-roadmap.md`](31-roadmap.md).

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
