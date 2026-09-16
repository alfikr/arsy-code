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

## Live resolution

`$CODEX_HOME/config.toml` and the repository's `.codex/config.toml` are read on
every launch by `arsy-compat` (`crates/arsy-compat/src/codex/`) and placed below
every `arsy.json` layer:

- `[mcp_servers]` become MCP connections, with `env`, `env_vars` (forwarded
  when set), `http_headers`, `env_http_headers`, `bearer_token_env_var`, and the
  longer of `startup_timeout_sec` and `tool_timeout_sec`. A repository file may
  not read the operator's variables into a header, and its servers start only
  in a trusted project.
- `sandbox_mode = "read-only"` asks before `fs.write` and `fs.delete`, or
  refuses them under `approval_policy = "never"`. `workspace-write` adds nothing
  and `danger-full-access` grants nothing. `approval_policy = "never"` itself is
  noted, not applied. `profiles` are not read.
- `model` is a fallback for an OpenAI endpoint that names none, narrowed to the
  endpoint whose id matches a non-default `model_provider`.
- `AGENTS.override.md` replaces `AGENTS.md` in a directory and in `$CODEX_HOME`.
- `notify` runs as `after_turn`, in `arsy run` and the TUI.

`[compat.codex] enabled = false` removes all of it.

## RPC strategy

Implement a strategically useful app-server façade after schema fixtures prove demand. Preserve initialization, thread/start/resume, turn/start, item lifecycle, approvals, and event ordering. Codex wire quirks stay inside the adapter. Unsupported experimental methods return explicit capability absence.

## Security and failure

Codex config cannot weaken enterprise/user policy. Sandbox modes map to the nearest equal-or-stronger ARSY profile or fail with an explanation. Imported provider credentials become secret handles. Session import validates schema/version and retains original artifacts for audit.

## Decision

Target zero-migration instruction/config use first, app-server client compatibility second, and full session migration only after stable fixtures. Do not reuse Codex domain types internally.
