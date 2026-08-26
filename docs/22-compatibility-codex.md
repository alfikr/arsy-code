# Codex compatibility

## Existing documented and verified surface

Codex source verifies a Rust app server, thread/turn/item protocol, provider/tool boundaries, thread storage, and sandboxes (**V**). Official documentation defines AGENTS.md discovery, `.codex/config.toml`, approval and sandbox settings, skills, MCP, and app-server behavior (**D**).

## Imports

Discover user and repository `AGENTS.md`/`AGENTS.override.md`, `.codex/config.toml`, skills, hooks where supported, MCP definitions, provider/model configuration, sandbox mode, and approval policy. Walk repository root to current directory using documented instruction semantics; translate each source to attributed fragments and canonical policy.

| Codex concept | ARSY target |
|---|---|
| thread | session projection |
| turn | turn |
| item | presentation item projection |
| rollout JSONL | import/export event stream, not native storage |
| approval policy | policy decision defaults |
| sandbox mode | requested assurance/resource profile |
| tool | model-facing operation view |
| app-server event | protocol adapter event |

## RPC strategy

Implement a strategically useful app-server façade after schema fixtures prove demand. Preserve initialization, thread/start/resume, turn/start, item lifecycle, approvals, and event ordering. Codex wire quirks stay inside the adapter. Unsupported experimental methods return explicit capability absence.

## Security and failure

Codex config cannot weaken enterprise/user policy. Sandbox modes map to the nearest equal-or-stronger ARSY profile or fail with an explanation. Imported provider credentials become secret handles. Session import validates schema/version and retains original artifacts for audit.

## Decision

Target zero-migration instruction/config use first, app-server client compatibility second, and full session migration only after stable fixtures. Do not reuse Codex domain types internally.
