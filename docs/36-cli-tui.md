# CLI and TUI surface

## Invocation

The executable is `arsy`. UTF-8 is required for task text and machine output. Arguments use platform-native paths; internally they are canonicalized before policy evaluation.

Global flags apply before or after a subcommand:

| Flag | Value/default | Description |
|---|---|---|
| `--workspace <PATH>` | current directory | workspace root |
| `--config <PATH>` | discovered native config | use one additional session-scoped config file; it cannot weaken policy |
| `--provider <ID>` | resolved default | select an allowed provider |
| `--model <ID>` | resolved default | select an allowed model |
| `--output <MODE>` | `human` on a TTY, `ci` otherwise | `human`, `json`, or `ci` |
| `--no-color` | false | disable ANSI styling; equivalent to `ui.color = "never"` |
| `--help` | — | print help and exit |
| `--version` | — | print version and target, then exit |

Unknown flags, missing arguments, invalid UTF-8, and invalid enum values are usage errors and never start a session.

## Commands

| Command | Positional arguments | Command flags | Description |
|---|---|---|---|
| `arsy` | none | global flags | open the interactive TUI in the workspace |
| `arsy run <TASK>` | one required task string; `-` reads it from stdin | global flags | execute one task non-interactively and exit at its terminal state |
| `arsy resume <SESSION_ID>` | one required canonical session ID | `--follow` plus global flags | resume an existing session; follow new events until terminal when requested |
| `arsy review [REVISION]` | optional Git revision; omitted means the working-tree diff | `--base <REVISION>` plus global flags | produce a structured review without modifying the workspace |
| `arsy mcp list` | none | global flags | list configured MCP connections, transports, trust labels, and enabled state without connecting |
| `arsy doctor` | none | `--strict` plus global flags | check configuration, credentials by handle, sandbox backends, Git, providers without a billable request, and release provenance; `--strict` turns warnings into failure |

`arsy mcp` without `list` prints its help and exits with usage status. `--base` is invalid unless `REVISION` is absent. `resume --follow` is implied in an interactive TTY and otherwise defaults off. Review and doctor are read-only; provider checks are non-destructive capability probes.

## TUI behavior

The TUI has a session timeline, task input, status line, evidence/diagnostic detail, and an approval view. It displays the active workspace, model route, session ID, achieved sandbox assurance, token/cost totals, and whether the result is degraded. Keyboard actions and screen-reader labels must expose every action available by pointer.

An approval view identifies the operation, canonical resource, exact scope, risk, policy source, expiry, and proposed assurance. The only decisions are deny, approve this operation, or approve the displayed bounded rule. Closing the view denies; repository content and model output cannot preselect approval.

The TUI renders canonical events and may reconnect from its last event cursor. It never owns session truth and never hides a terminal diagnostic behind a transient notification.

## Output modes

| Mode | Standard output | Standard error | Presentation |
|---|---|---|---|
| `human` | final answer or requested listing | progress, approvals, warnings, diagnostics | localized prose, optional ANSI, progress redraw allowed |
| `json` | UTF-8 NDJSON records | bootstrap failures before JSON initialization only | no ANSI; each record has `schema_version`, `type`, `sequence`, `session_id`, and typed payload |
| `ci` | final answer or listing | stable `ARSY <SEVERITY> <CODE> <MESSAGE>` lines | no ANSI, spinner, cursor control, localization, or interactive prompt |

JSON record types are `event`, `result`, and `diagnostic`. Exactly one terminal `result` is emitted after initialization, even on failure; diagnostics use the fields in [the stable taxonomy](33-diagnostics.md). Machine modes never mix human prose into structured stdout. Secret values are redacted in every mode.

## Exit codes

The process returns the class-specific code for the terminal diagnostic. Warnings with a valid degraded result return `0` unless `doctor --strict` is active.

| Exit | Meaning | Diagnostic classes |
|---|---|---|
| `0` | completed, possibly with reported warnings | none |
| `2` | invalid CLI/config/schema input | `SCH` |
| `3` | policy denial or approval unavailable | `POL` |
| `4` | required sandbox assurance unavailable | `SBX` |
| `5` | provider or protocol failure | `PRV`, `PRT` |
| `6` | operation, tool, edit, or stale-write failure | `EXE`, `TLS`, `EDT`, `STL` |
| `7` | verification failed; completion blocked | `VER` |
| `8` | compaction, coordination, storage, or internal integrity failure | `CMP`, `CRD` |
| `9` | retrieval, context, planning, or unsupported model result | `RET`, `CTX`, `PLN`, `MDL` |
| `10` | requested interface cannot present a usable result | `UIX` |
| `130` | interrupted by the user | terminal interrupt diagnostic |

If several failures contribute, the terminal/primary diagnostic selects the exit code and contributing diagnostics remain in output. Signals and Windows control events are normalized to `130` only for an acknowledged user interrupt.

## Non-interactive and approval behavior

With no TTY, bare `arsy` fails with exit `2` and directs the caller to `arsy run`; it never guesses a task. `run`, machine-output modes, and non-TTY `resume --follow` never display or wait on a terminal approval prompt.

When policy returns `ask` and no authenticated approval channel is attached, ARSY emits an `ARSY-POL-*` diagnostic naming the rule and required approval, records the operation as not executed, and exits `3`. There is no implicit approval, weaker sandbox fallback, or environment variable that bypasses this rule. Piped stdin supplies task text only and conveys no authority.
