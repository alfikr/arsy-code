# Research report and claim ledger

## Method and scope

Research was performed 2026-08-26 against primary repositories cloned locally and official protocol/product documentation. Source claims use permanent commit links. Search results and READMEs are discovery aids; implementation claims require source. The public Claude Code repository does not contain its core runtime, so its runtime claims are **D**, not **V**.

Pinned revisions:

- Oh My Pi: [`eab72e88`](https://github.com/can1357/oh-my-pi/tree/eab72e88e447a4be45bea2bc302995844c0c51a2), 6,659 tracked files.
- OpenAI Codex: [`21c58c90`](https://github.com/openai/codex/tree/21c58c90f2298587c6519e077d0692ce4c563d37), 6,667 tracked files and 144 `Cargo.toml` files under `codex-rs`.
- Claude Code: [`005c5dad`](https://github.com/anthropics/claude-code/tree/005c5dade90c2c59c88d819d8723e7b579addb5e), 229 tracked files; no tracked `Cargo.toml` or `package.json` was found within four levels.

Counts are reproducible observations from `git ls-files` and `find`; they describe these revisions, not product size or quality.

## Claim ledger

| ID | Class | Claim and evidence | Architectural implication |
|---|---|---|---|
| OMP-EDIT | V | Hashline binds line tags to content, validates live state, preflights multi-hunk edits, and supports session recovery: [README](https://github.com/can1357/oh-my-pi/blob/eab72e88e447a4be45bea2bc302995844c0c51a2/packages/hashline/README.md). | Use multi-address edits with optimistic concurrency, not blind replacement. |
| OMP-LSP | V | OMP implements an LSP tool with read/write approval separation and definition, references, hover, code actions, symbols, rename, and reload: [source](https://github.com/can1357/oh-my-pi/blob/eab72e88e447a4be45bea2bc302995844c0c51a2/packages/coding-agent/src/lsp/tool.ts). | Put semantic operations above raw LSP. |
| OMP-DAP | V | Its debug tool implements launch/attach, breakpoints, stepping, stack, scopes, variables, evaluation, memory, and session control: [source](https://github.com/can1357/oh-my-pi/blob/eab72e88e447a4be45bea2bc302995844c0c51a2/packages/coding-agent/src/tools/debug.ts). | DAP is a credible first-class agent capability. |
| OMP-SESSION | V | Sessions are append-oriented JSONL with branches, migration, compaction entries, and blobs; storage notes explicitly distinguish atomic writes from durable `fsync`: [design](https://github.com/can1357/oh-my-pi/blob/eab72e88e447a4be45bea2bc302995844c0c51a2/docs/session.md), [storage](https://github.com/can1357/oh-my-pi/blob/eab72e88e447a4be45bea2bc302995844c0c51a2/packages/coding-agent/src/session/session-storage.ts). | Preserve lineage but strengthen transactions, projections, and durability. |
| OMP-COMPAT | V | Context discovery imports native, Claude, Codex, Gemini, OpenCode, GitHub, and AGENTS conventions with scope/depth precedence: [design](https://github.com/can1357/oh-my-pi/blob/eab72e88e447a4be45bea2bc302995844c0c51a2/docs/context-files.md). | Compatibility discovery is useful, but belongs at ingestion. |
| OMP-RUNTIME | V | The repository is a Bun/TypeScript workspace plus Rust native crates and multiple model dialect modules. | Provider breadth is valuable; split runtimes enlarge release and security surface. |
| CODEX-STORE | V | `ThreadStore` separates raw canonical history, metadata, live handles, JSONL rollout files, and SQLite querying: [README](https://github.com/openai/codex/blob/21c58c90f2298587c6519e077d0692ce4c563d37/codex-rs/thread-store/README.md). | Adopt a storage boundary and rebuildable projections. |
| CODEX-TRACE | V | Rollout trace records ordered runtime events and payload references, then derives a graph offline: [README](https://github.com/openai/codex/blob/21c58c90f2298587c6519e077d0692ce4c563d37/codex-rs/rollout-trace/README.md). | Separate canonical events from diagnostic projections. |
| CODEX-SANDBOX | V | Linux uses bubblewrap/namespaces/seccomp with a Landlock legacy path; source also contains macOS Seatbelt and Windows restricted-token/Job Object mechanisms: [Linux design](https://github.com/openai/codex/blob/21c58c90f2298587c6519e077d0692ce4c563d37/codex-rs/linux-sandbox/README.md). | Separate portable policy from platform enforcement. |
| CODEX-PROVIDER | V | `ModelProvider` owns provider metadata, capabilities, authentication, error mapping, and wire adaptation: [source](https://github.com/openai/codex/blob/21c58c90f2298587c6519e077d0692ce4c563d37/codex-rs/model-provider/src/provider.rs). | Retain provider ownership but make capabilities granular and probed. |
| CODEX-TOOLS | V | `ToolExecutor` couples model-visible spec and executable runtime while allowing host routing/hooks: [source](https://github.com/openai/codex/blob/21c58c90f2298587c6519e077d0692ce4c563d37/codex-rs/tools/src/tool_executor.rs). | ARSY splits model schema, operation, policy, renderer, and runtime. |
| CODEX-APP | V+D | The open app server exposes versioned schemas and thread/turn/item/approval flows: [source](https://github.com/openai/codex/blob/21c58c90f2298587c6519e077d0692ce4c563d37/codex-rs/app-server/README.md), [official guide](https://developers.openai.com/codex/app-server/). | Make the service protocol the UI boundary. |
| CLAUDE-SURFACE | D | Official docs define CLAUDE.md, skills, agents, hooks, plugins, MCP, settings, and permissions: [features](https://code.claude.com/docs/en/features-overview), [settings](https://code.claude.com/docs/en/settings), [hooks](https://code.claude.com/docs/en/hooks). | Reproduce user-facing import semantics where feasible, not unseen internals. |
| CLAUDE-REPO | V | The public repository contains release material, plugin examples, hook manifests, and scripts, but no inspected core runtime implementation: [repository](https://github.com/anthropics/claude-code/tree/005c5dade90c2c59c88d819d8723e7b579addb5e). | Never label runtime architecture as verified. |
| MCP | D | MCP 2026-07-28 uses JSON-RPC 2.0, hosts/clients/servers, negotiated tools/resources/prompts/elicitation, optional extensions, and treats tool annotations as untrusted: [specification](https://modelcontextprotocol.io/specification/2026-07-28). | Implement bidirectional adapters behind ARSY policy. |
| ACP | D | ACP v1 defines initialize/authenticate/session flows, permission requests, filesystem and terminal client capabilities, absolute paths, and negotiated extensions: [overview](https://agentclientprotocol.com/protocol/v1/overview). | Map ACP sessions and updates to protocol projections. |
| LSP | D | LSP 3.17 standardizes lifecycle, synchronization, language, and workspace features: [specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/). | Host servers once; expose task-oriented semantic operations. |
| DAP | D | DAP uses adapter processes, capability negotiation, launch/attach, and framed JSON messages: [overview](https://microsoft.github.io/debug-adapter-protocol/overview). | Treat adapter capabilities as runtime facts, not assumptions. |

## Gaps and limits

- Claude runtime internals, hosted Codex services, Cursor internals, and vendor evaluation corpora are unavailable; the design does not infer them.
- Compatibility promises require fixture-based conformance tests; current documents specify targets, not achieved compatibility.
- Provider capability catalogs drift. Runtime probes and dated overrides are required before execution.
- OS sandbox equivalence is impossible: each platform receives explicit guarantees and unsupported states fail closed.
