# OMP / Pi compatibility

## Verified surface

At the pinned revision, OMP implements multi-format context discovery, provider/dialect configuration, skills/agents, task isolation, memory/session features, MCP transports, RPC/ACP-related integration, LSP, DAP, and hashline editing (**V**). See [claim ledger](report-source.md).

## Imports and mappings

Discover `.omp/` and supported Pi configuration, rule/context files, skills, agent definitions, provider/model settings, memory exports, extension declarations, MCP servers, and context-file formats. Preserve source order and report duplicates.

| OMP concept | ARSY target |
|---|---|
| hashline edit | content-anchor edit address |
| LSP/DAP tool | semantic operation / debug capability |
| task agent | role + task node + agent runtime |
| advisor/watchdog | observer subscription |
| session JSONL/blob | event/artifact importer |
| provider dialect | model prompt/tool/wire strategy |
| context URI | resource reference where semantics are retained |
| RPC/ACP | edge protocol adapter |

## Behavioral strategy

Reuse context discovery semantics only through fixtures; ARSY-native config wins for equivalent repository intent, while enterprise/user policy always constrains effects. Hashline-compatible anchors may be accepted directly. Imported session trees retain parentage and original payload artifacts; unsupported entry types remain opaque rather than discarded.

## Security and limitations

TypeScript extensions do not execute in the core. They may run in an explicitly approved external compatibility worker with scoped capabilities, or be rejected. Provider quirks are translated to model profiles but never copied into operations. Internal URL schemes are mapped only when they provide stable identity; scheme-specific behavior remains typed.

## Decision and open questions

Prioritize instruction/skill/agent/provider/MCP imports and hashline interoperability. Full extension and session behavioral parity is P3 and must be justified by real adoption data.
