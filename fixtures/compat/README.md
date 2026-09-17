# Compatibility golden fixtures

These repositories pin Phase 0 import behavior before adapter implementation. Run `python3 fixtures/compat/check.py` from the repository root.

Each fixture contains:

- `input/`: files exactly as an external harness would discover them;
- `expected/canonical.json`: normalized, authority-free import output;
- `expected/loss.json`: explicit `parsed`, `mapped`, `behavior-tested`, or `unsupported` fidelity results;
- `README.md`: working directory, precedence case, and the behavior under test.

Canonical output is declarative data only. A fixture may request capabilities, hooks, plugins, or MCP processes, but loading the fixture never executes them and never grants authority. Source paths are relative to the fixture directory so failures remain readable.

| Fixture | Pinned semantics |
|---|---|
| `agents` | root-to-working-directory `AGENTS.md` walk and nearest override |
| `claude` | nested instructions, project/local settings, skill, hook, agent, plugin, MCP |
| `codex` | AGENTS override, TOML approval/sandbox mapping, skill, MCP |
| `omp` | nearest native context, YAML settings, skill, agent, extension quarantine, MCP |
| `live` | Claude Code and Codex read live: MCP precedence, trust, and secrets; permissions; models; user instructions. Pinned by `crates/arsy-compat/tests/live_golden.rs` in `expected/resolved.json` |

The source behavior is bounded by [the compatibility specifications](../../docs/21-compatibility-claude.md), [Codex mapping](../../docs/22-compatibility-codex.md), [OMP mapping](../../docs/23-compatibility-omp.md), and the pinned revisions in [the claim ledger](../../docs/report-source.md).
