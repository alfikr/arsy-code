# Competitive research

## Method

Evidence classes and pinned revisions are in [report-source.md](report-source.md). **V** means source-inspected, **D** publisher-documented, **I** inference, and **P** our decision.

## Oh My Pi

**Existing approach.** OMP combines a Bun/TypeScript agent stack with Rust native crates (**V**), broad provider dialect handling (**V**), content-hash line edits (**V**), LSP and DAP tools (**V**), agent/advisor workflows (**V**), compatibility context discovery (**V**), and append-oriented sessions (**V**).

**Strengths.** Hashline attacks stale writes directly. LSP/DAP breadth demonstrates that models can consume richer developer primitives. Its compatibility discovery and model dialect work expose real ecosystem variance.

**Weaknesses.** Multiple runtime/toolchains increase packaging and supply-chain surface (**I**). JSONL lineage is understandable, but transactional indexing and explicit durability need another layer (**V+I**). Provider quirks are necessarily widespread across dialect and request code (**V**), creating evolutionary pressure.

**Take.** Adopt semantic tooling, debugger access, content-bound edits, model strategies, and dynamic observers. Replace runtime splits with Rust core boundaries and capability mediation.

## OpenAI Codex

**Existing approach.** Codex has an extensive Rust workspace (**V**), a model-provider trait (**V**), executable tool contracts (**V**), app-server protocol (**V+D**), thread storage and rollout traces (**V**), and platform-specific sandbox implementations (**V**).

**Strengths.** Strong process/security engineering, explicit client/server separation, structured thread/turn/item events, and queryable persistence. The repository shows serious cross-platform and protocol investment.

**Weaknesses.** The inspected `ToolExecutor` retains a direct relationship between model-visible specification and runtime (**V**). Semantic LSP/DAP capabilities are not comparable to OMP’s inspected surface at the pinned revision (**V**, absence claim bounded to searched source). A 144-manifest Rust tree is evidence of scale, not automatically good boundaries.

**Take.** Adopt Rust-first service architecture, storage boundary, approval flow, and mechanism-specific sandboxes. Generalize tool permission into typed effects and keep semantic code intelligence first-class.

## Claude Code

**Existing approach.** Official documentation describes strong repository instructions, settings precedence, skills, agents, hooks, plugins, permissions, MCP, and worktree-isolated agents (**D**). The public repository provides plugin and hook examples but not core runtime source (**V**).

**Strengths.** Discoverable authoring conventions, progressive skill loading, approachable hook/plugin packaging, and mature permission UX (**D**).

**Weaknesses.** Provider-neutrality and runtime extension isolation cannot be verified publicly. The ecosystem semantics are portable only through documented formats, and native behavior may change without an inspectable implementation.

**Take.** Import the UX surface faithfully where tested; never mirror undocumented internals. Execute translated hooks/plugins under ARSY policy.

## Standards and secondary references

MCP is a valuable integration wire but its own specification says tool annotations are untrusted (**D**). ACP is a useful IDE-agent interoperability edge (**D**). LSP and DAP solve language/debug adapter multiplication (**D**). Git worktrees provide mature isolation but share repository object/ref machinery unless carefully managed. SQLite WAL provides transactional local storage with concurrent readers and one writer.

## Comparative decision matrix

| Question | OMP evidence | Codex evidence | Claude evidence | ARSY decision |
|---|---|---|---|---|
| Canonical state | JSONL tree + blobs | thread store + rollout/SQLite | unavailable | immutable events + CAS + projections |
| Editing | hashline/text/LSP | patch/file-change tools | documented tools only | transactional hybrid engine |
| Semantics | LSP + DAP | limited inspected equivalent | LSP plugins documented | semantic capability layer |
| Security | approvals/isolation features | strong OS mechanisms | permissions/hooks documented | capability policy + platform mechanism |
| Extensibility | TS extensions/tools | Rust/plugins/MCP surfaces | plugins/hooks/skills | data/skills + WASM + external protocols |
| UI boundary | RPC/ACP | app server | unavailable | canonical service protocol |
| Provider adaptation | broad dialect modules | provider trait + OpenAI-oriented paths | Anthropic | capability-probed strategy pipeline |

## Open questions

Conformance depth for proprietary behavior, provider probe safety, Windows filesystem mediation, and cost-effective semantic graph freshness remain empirical questions assigned to roadmap gates.
