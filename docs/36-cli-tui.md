# CLI and TUI surface

## Implemented integration workflows

The command tables below include roadmap work. The current build provides
`arsy mcp list`, `arsy mcp show <NAME>`, and `arsy hook list` as read-only
inspection commands. MCP inspection reads workspace Claude `.mcp.json`, Codex
`.codex/config.toml`, and the nearest OMP `.omp/mcp.json`; `--source
claude|codex|omp` selects one ecosystem and resolves duplicate names. It preserves
stdio/HTTP transport and explicit enabled state without launching a server.
Hook inspection reads Claude workspace and local settings, preserves lifecycle
events and matchers, and accepts `--event <original-or-canonical-name>`.
Unsupported lifecycle events are labelled unsupported. Other hook ecosystems
and user/enterprise integration configuration are not yet inspected.

Every declaration is labelled `not_loaded`. Native MCP connection management,
handshakes, discovery, reconnect/refresh, and executable hooks remain unimplemented;
these inspection results are not a claim that an integration is running.
The existing provider subprocess owns its own integrations and permissions.

Human output lists each declaration as a count line and one row per declaration:
its name, transport or matcher, `not loaded`, its source file and ecosystem, the
command or URL a connection would run (hooks report lifecycle, handler types, and
effect), and its trust and mapping level. `--output json` carries the full record.
An empty result names the files that were read and the filters that were applied.

In a TUI build (`cargo build -p arsy-cli --features tui`), use `/help`, `/mcp`,
`/mcp show NAME --source claude`, or `/hooks --event PreToolUse`. The same route
carries the other read-only inspections under their own names: `/settings [KEY]`
for `config explain`, `/doctor`, `/auth` for the credential listing, and
`/compat claude|codex|omp|agents`. Each expands to the CLI command it stands for
and is parsed by the same grammar, so an unsupported argument is refused with the
CLI's diagnostic. Every command on this route is read-only — `auth set`,
`auth login`, and `auth remove` are not reachable from the TUI — so `/model`,
which writes the accepted answer to the user configuration, remains the only
slash command that changes state. These work even without provider
authentication. Repeat an inspection to reload its source files. Unknown slash
commands report an error instead of becoming model prompts.

Typing `/` opens a command menu under the composer, one row per command with its
description, narrowed as the line is typed and closed once an argument follows.
Up/Down move the marked row while it is open, and Enter takes the marked command
into the line; a line that already is a command is sent instead, so `/quit` never
has to be chosen from a list. `/help` prints the same table, and both read one
source, so a command cannot appear in one and not the other. The menu is not
offered at the model picker, which collects an answer rather than a command, and
it takes only the rows the terminal has left over the four the composer always
paints, so the input block never outgrows the screen.
Use Up/Down for the last 100 submitted lines when no menu is open, Delete for
forward deletion, and bracketed paste to insert text without submitting pasted
newlines. History is in memory only; multiline paste becomes spaces in the
single-line composer.

`/effort low|medium|high` sets how much reasoning a turn asks for, `/effort off`
clears it, and a bare `/effort` reports the current level without changing it.
The choice is remembered beside the model. Unset is the default and sends no
reasoning field at all, so a host without such a model sees the request it always
saw. The two dialects spend it differently: Chat Completions takes the level by
name as `reasoning_effort`, while Messages takes a share of `max_tokens` as a
thinking budget, floored at the 1024 tokens the API requires and omitted when the
output budget cannot hold both the floor and an answer. A scripted `arsy run`
ignores the remembered level and sends no reasoning field, so a pipeline cannot
change behaviour because of an interactive choice made elsewhere.

`/model` reopens the picker. It accepts a list number, a model slug, or an empty
line to keep the current model; anything else — a mistyped slash command, an out
of range number, a slug with whitespace — is rejected with a reason and the
picker stays open, because an accepted answer is also written to the user
configuration. A remembered model is re-validated on read, so a file written by
an older build cannot keep selecting an unusable model. Esc, Ctrl-C, or Ctrl-D at
the picker leaves the model unchanged and returns to the task prompt.

While a turn runs, the composer shows elapsed time and queued follow-ups (up to
16 per running turn). MCP started/updated/completed events and failure details
appear above it. Esc/Ctrl-C cancels the turn and clears pending follow-ups;
Ctrl-D on empty input cancels and exits. Cancellation escalates after two seconds.
Provider turns currently have a fixed 300-second deadline and a 1 MiB per-event
limit. The terminal is restored on exit and provider processes are cleaned up on
I/O errors. JSON/CI output requires an explicit non-interactive command.

Checks: `cargo test -p arsy-cli --features tui`,
`cargo test -p arsy-code --test compat_golden`, and
`python3 crates/arsy-cli/tests/tui_smoke.py target/debug/arsy` (Unix, TUI build).
Any workspace-wide cargo command rebuilds `target/debug/arsy` without the `tui`
feature, so run `cargo build -p arsy-cli --features tui` immediately before the
smoke test; it reports a stale binary rather than timing out on it.

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

Every command accepts the global flags above and honours the output modes and exit codes below.
**Availability** names the roadmap phase that first ships the command; a command listed here but not
yet available exits `2` with an `ARSY-SCH-*` diagnostic naming its phase, never a generic parse error.

Commands whose name is `list`, `show`, `explain`, `inspect`, or `test`, plus `doctor` and `eval`, are
read-only: they never mutate the workspace, session history, or stored configuration.

### Session

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy` | none | global flags | open the interactive TUI in the workspace | 2 |
| `arsy run <TASK>` | one required task string; `-` reads it from stdin | global flags | execute one task non-interactively and exit at its terminal state | 1 |
| `arsy resume <SESSION_ID>` | one required canonical session ID | `--follow` plus global flags | resume an existing session; follow new events until terminal when requested | 1 |
| `arsy review [REVISION]` | optional Git revision; omitted means the working-tree diff | `--base <REVISION>` plus global flags | produce a structured review without modifying the workspace | 6 |
| `arsy session list` | none | `--workspace-only`, `--limit <N>` | list session IDs with workspace, status, start time, and token totals | 1 |
| `arsy session show <SESSION_ID>` | one required session ID | `--turns`, `--evidence` | show turns, recorded evidence, approvals, and totals for one session | 1 |
| `arsy session export <SESSION_ID>` | one required session ID | `--out <PATH>`, `--include-artifacts` | export canonical events as JSONL for audit or forensic review | 1 |
| `arsy session rewind <SESSION_ID>` | one required session ID | `--to <EVENT_ID>` required | create a new branch pointing at an earlier event; never truncates history | 1 |
| `arsy session fork <SESSION_ID>` | one required session ID | `--at <EVENT_ID>` | start a new session recording ancestry from an existing one | 1 |

### Configuration and policy

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy config explain [KEY]` | optional dotted key; omitted explains every key | `--source-only` | show the effective value, the layer that supplied it, the merge strategy, and the rejected candidates | 1 |
| `arsy compat explain <ECOSYSTEM>` | one of `claude`, `codex`, `omp` | `--loss-only` | show discovered sources, precedence, canonical mapping, and the loss report | 5 |
| `arsy policy explain <OPERATION>` | one canonical operation kind | `--resource <REF>`, `--actor <ID>` | evaluate a policy query and print the decision, deciding rule, and policy source without executing anything | 1 |

### Credentials and models

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy auth set <PROVIDER>` | one configured provider ID | `--handle <NAME>` | read a secret from a no-echo prompt, or from stdin when piped, store it in the OS credential store, and print only the resulting handle | 1 |
| `arsy auth login <PROVIDER>` | one configured provider ID | global flags | sign in through the OAuth client the provider's configuration names, using the device grant when it offers one and the authorization-code grant with PKCE otherwise, and store the resulting token set under the provider's handle | 1 |
| `arsy auth list` | none | global flags | list stored credential handles with provider, creation time, and last use; never the secret value | 1 |
| `arsy auth remove <HANDLE>` | one required handle | `--force` | delete a stored credential and report the configuration keys that referenced it | 1 |
| `arsy provider list` | none | `--all` | list providers resolved as allowed, with the ceiling that narrowed them | 1 |
| `arsy model list` | none | `--provider <ID>`, `--capability <NAME>` | list allowed models with tri-state capabilities, capability source, and observation date | 2 |

### Connections

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy mcp list` | none | global flags | list configured MCP connections, transports, trust labels, and enabled state without connecting | 5 |
| `arsy mcp add <NAME>` | one connection name | `--transport <stdio\|http>`, `--command`, `--url`, `--scope <user\|workspace>` | write a connection definition; `--scope` defaults to `user` | 5 |
| `arsy mcp remove <NAME>` | one connection name | `--scope <user\|workspace>` | remove a connection definition from the named scope | 5 |
| `arsy mcp enable <NAME>` / `arsy mcp disable <NAME>` | one connection name | `--scope <user\|workspace>` | toggle a connection without deleting its definition | 5 |
| `arsy mcp test <NAME>` | one connection name | `--timeout <SECONDS>` | connect, negotiate capabilities, and disconnect; never invokes a tool | 5 |
| `arsy mcp reconnect <NAME>` | one connection name, or none with `--all` | `--all`, `--timeout <SECONDS>` | restore a dropped live connection and re-apply its capability ceiling | 5 |
| `arsy mcp refresh <NAME>` | one connection name, or none with `--all` | `--all` | re-run tool, resource, and prompt discovery without tearing the connection down | 5 |

### Extensions

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy plugin list` | none | `--capabilities` | list installed plugins with version, signature status, and granted capabilities | 8 |
| `arsy plugin install <SOURCE>` | one path or registry reference | `--scope <user\|workspace>` | display the requested capabilities and install only on explicit confirmation | 8 |
| `arsy plugin inspect <ID>` | one plugin ID | global flags | show the manifest, requested and granted capabilities, limits, and publisher identity | 8 |
| `arsy plugin remove <ID>` | one plugin ID | `--force` | uninstall a plugin and revoke its grants | 8 |
| `arsy plugin refresh [ID]` | optional plugin ID; omitted refreshes every source | `--dry-run` | reload plugins, skills, and hooks from their sources, effective at the next turn boundary | 8 |
| `arsy skill list` | none | `--source` | list loaded skills with their originating layer and authority class | 5 |
| `arsy hook list` | none | `--event <NAME>` | list registered hooks with lifecycle event, declared effect class, and origin | 8 |

### Evidence

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy artifact show <REF>` | one `artifact://` reference | `--max-bytes <N>` | render a bounded, redacted excerpt with the artifact's metadata | 1 |
| `arsy artifact export <REF>` | one `artifact://` reference | `--out <PATH>` required | write the artifact to a caller-named path and report what redaction removed | 1 |

### Maintenance

| Command | Positional arguments | Command flags | Description | Availability |
|---|---|---|---|---|
| `arsy doctor` | none | `--strict` | check configuration, credentials by handle, sandbox backends, Git, providers without a billable request, and release provenance; `--strict` turns warnings into failure | 1 |
| `arsy migrate <TARGET>` | one of `config`, `session` | `--apply`, `--backup <PATH>` | report the planned migration and its loss report; `--apply` is required to write | 1 |
| `arsy gc` | none | `--apply`, `--retention <DURATION>` | report artifacts unreachable and past retention; `--apply` is required to delete | 1 |
| `arsy serve` | none | `--transport <stdio\|socket>` | serve the canonical protocol for an embedding client; defaults to stdio and is never a background daemon | 1 |
| `arsy eval <SUITE>` | one suite path or ID | `--trials <N>`, `--out <PATH>` | run an evaluation suite and report outcome, efficiency, and safety metrics | 1 |
| `arsy completions <SHELL>` | one of `bash`, `zsh`, `fish`, `powershell` | none | print a shell completion script to standard output | 1 |

### Command rules

A group name used without a subcommand — `arsy mcp`, `arsy session`, `arsy auth`, `arsy plugin`,
`arsy artifact`, `arsy config`, `arsy compat`, `arsy policy` — prints its help and exits with usage
status. `--base` is invalid unless `REVISION` is absent. `resume --follow` is implied in an
interactive TTY and otherwise defaults off.

Bare `arsy` prefers a configured provider endpoint and falls back to a logged-in Codex CLI when
nothing is configured, so `/model` offers Codex's cached list on that route and takes a slug as
free text on a configured one. The remembered choice is stored as `provider/model` and only applies
to the provider it was chosen for.

`arsy auth login` prints the URL to visit rather than opening a browser, because an operator working
over SSH is not looking at a browser on the machine that ran the command.

`arsy auth set` never accepts a secret as an argument, because arguments reach the process list and
shell history. When no credential store is available it fails; it never falls back to plaintext
storage. No command prints a stored secret in any output mode.

`arsy mcp reconnect` repairs a live connection and re-applies its capability ceiling; `arsy mcp test`
is a separate probe that connects and disconnects without touching the session. Neither accepts a
capability the connection did not already hold: a tool that appears only after reconnecting and falls
outside the ceiling is rejected, not adopted.

`arsy plugin refresh` reloads plugins, skills, and hooks, takes effect at the next turn boundary
rather than immediately, and refuses any source whose manifest requests wider capabilities than were
approved at install. `--dry-run` reports what would change and loads nothing.

`arsy migrate` and `arsy gc` report without writing unless `--apply` is given, so a forgotten flag
cannot destroy data. `arsy migrate` takes a verified backup before applying and leaves the original
store openable if it fails.

`arsy mcp add` writes to the user `config.toml` by default and to `.arsy/config.toml` under
`--scope workspace`. A definition written at either scope remains untrusted content: it declares a
connection, and grants no capability.

ARSY has no update command. Updates and rollbacks are handled by the installation channel, as
specified in [distribution](34-distribution.md).

## TUI behavior

The status line carries the model route, the reasoning effort (`effort:—` when
unset), the checked-out branch when the workspace is a Git checkout, and the
workspace path. The branch is read from `.git/HEAD` once per prompt, so a
checkout made in another terminal appears on the next line rather than at the
next restart.

The TUI has a session timeline, task input, status line, evidence/diagnostic detail, and an approval view. At startup it detects a logged-in Codex installation through `codex login status`, then asks for a model; an empty selection uses the Codex default. Codex credentials and configuration remain owned by Codex and are never copied into ARSY. Entering a task runs it through Codex in read-only mode and returns to the task prompt; `:quit` or end-of-file exits. It displays the active workspace, model route, session ID, achieved sandbox assurance, token/cost totals, and whether the result is degraded. Keyboard actions and screen-reader labels must expose every action available by pointer.

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
