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

## Security, compatibility, versioning

Signatures establish publisher identity, not safety. Install shows requested capabilities; updates cannot expand them silently. [`arsy plugin`](36-cli-tui.md) surfaces install, inspection, and removal, and `arsy skill list` and `arsy hook list` report what is loaded. Claude/OMP plugins are parsed into declarative pieces; unsupported executable behavior is quarantined or requires an external compatibility runner. Plugin API versions independently; host imports are capability- and version-negotiated.

## Decision

WASM is the default compute extension boundary. Skills and prompts remain cheap data. Native in-process ABI is an anti-goal.
