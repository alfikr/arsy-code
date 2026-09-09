# Plugin and extension system

## Problem and existing approaches

Claude documents a productive bundle of skills, commands, agents, hooks, MCP, LSP, and plugin manifests (**D**). OMP exposes extensibility across its runtime (**V**). Arbitrary in-process extension code can bypass every core guarantee.

## Layers

1. Data-only: instructions, skills, prompts, schemas, themes.
2. Declarative: commands, workflows, hook rules, MCP/LSP/DAP declarations.
3. Sandboxed compute: WASM components with explicit imports.
4. External process/protocol: MCP, ACP, language/debug servers under capability policy.
5. Trusted native: disabled by default, out of process, administrator-installed only.

```toml
manifest_version = 1
id = "example.review"
version = "1.2.0"
entrypoint = "plugin.wasm"
api = ">=1,<2"
capabilities = ["fs.read:workspace/**"]
```

```rust
pub trait ExtensionHost {
    fn invoke(&self, plugin: PluginId, entry: Entrypoint,
              input: ArtifactRef, grant: CapabilityGrant)
        -> BoxFuture<'_, Result<ArtifactRef, ExtensionError>>;
}
```

Wasmtime/WASI components receive preopened capability resources via `cap-std`, fuel/time/memory limits, deterministic clocks/randomness where requested, and bounded output. Network, secrets, process spawn, and host filesystem are absent unless declared and granted.

## Hooks

Lifecycle events include harness/session/turn/model/operation/edit/command/commit/agent/compaction stages. Hooks may observe, transform a schema-limited payload, deny, request approval, inject attributed context, or schedule one follow-up. Recursion depth, reentrancy keys, timeout, and origin prevent loops. Hook failure policy is event-specific and explicit.

This build dispatches `before_turn`, `before_operation`, `after_operation`, `operation_failed`, and `after_turn`. A declaration is loaded from the operator's own configuration — Claude's, Codex's, or ARSY's — and from the repository's only where `[project."<path>"] trust_level = "trusted"` vouches for it. See [`arsy hook list`](36-cli-tui.md).

## Refresh

Layer 1 and layer 2 sources — instructions, skills, prompts, schemas, commands, hook rules, and
adapter declarations — are re-read from their origin on refresh. A WASM component is reloaded as a
new module version; the running instance is drained rather than killed mid-invocation.

Refresh takes effect at a turn boundary and never inside one. A turn finishes with the extension set
it began with, so a hook or skill cannot change the rules under an agent that is already applying
them.

Refresh cannot widen capability. A manifest whose requested capabilities exceed what was granted at
install is not loaded; it requires explicit approval through `arsy plugin install`, exactly as an
update does. Without this rule refresh would be the way around the update restriction above.

A failed refresh keeps the previously loaded version. One malformed source never leaves the host with
no extensions, and each failure is reported against the source that caused it rather than as a single
opaque error.

Every refresh appends an event naming which sources changed, which were rejected, and why.

## Security, compatibility, versioning

Signatures establish publisher identity, not safety. Install shows requested capabilities; updates cannot expand them silently. [`arsy plugin`](36-cli-tui.md) surfaces install, inspection, and removal, and `arsy skill list` and `arsy hook list` report what is loaded. Claude/OMP plugins are parsed into declarative pieces; unsupported executable behavior is quarantined or requires an external compatibility runner. Plugin API versions independently; host imports are capability- and version-negotiated.

## Decision

WASM is the default compute extension boundary. Skills and prompts remain cheap data. Native in-process ABI is an anti-goal.
