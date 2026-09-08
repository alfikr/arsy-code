# Harness core: what is built, and how to extend it

This is the *as-built* description of the layer between a model and the
workspace. [`04-system-architecture.md`](04-system-architecture.md) describes
the target the project is heading for; this describes what is in `main` now, so
a contributor does not have to reverse-engineer it.

Everything here lives in `crates/arsy-code/src/agent/`.

## The one path

There is exactly one route from a model tool call to an effect. Nothing —
not the TUI, not `arsy run`, not `arsy serve` — has another.

```text
model tool call            name + JSON arguments as the provider sent them
      │
      │  ToolRuntime::prepare  →  agent::TOOLS lookup, argument translation
      ▼
OperationRequest           kind, actor, and the capability requirements the
      │                    call needs over concrete resources
      │  ToolRuntime::authorize  →  RuleSet::evaluate, once per requirement
      ▼
Authorization              Allowed(grants) | NeedsApproval{..} | Denied(reason)
      │
      │  ToolRuntime::dispatch  →  OperationRegistry::dispatch
      │                            (schema check, grant check, execute)
      ▼
OperationOutcome           result and evidence, stored as artifacts
      │
      │  ToolRuntime::render  →  read the artifact back, render, truncate
      ▼
ToolResult                 { success, output, changed_files, metadata, artifact }
```

Two properties follow from having one path, and both are worth protecting:

- **A capability decision is made once.** Adding a front end cannot add a way
  around policy, because a front end only ever produces an `OperationRequest`.
- **A tool call is replayable.** Every executor writes its result to the
  artifact store rather than returning it inline, so `ToolResult.artifact`
  points at the whole thing even after the transcript has been trimmed.

## Layers

| Concern | Where | Note |
|---|---|---|
| Model-facing tools | `agent::TOOLS` | name, description, JSON Schema, argument translation. No policy, no state, no I/O. |
| Execution + authority | `agent::ToolRuntime` | the diagram above |
| Transcript budget | `agent::budget` | which observations stay verbatim |
| Instruction loading | `agent::instructions` | which Markdown reaches the model |
| File operations | `agent::fsops` | `fs.read/list/write/create/edit/delete/move` |
| Patch | `agent::patch` | the `*** Begin Patch` dialect |
| Search | `agent::searchops` | `search.files`, `search.text` |
| Shell | `crate::process` | `process.exec`, sandbox-aware, workspace cwd |
| Git | `crate::git` | `git.status/diff/log/blame` |
| Path confinement | `crate::resource::Workspace` | the only way to touch a workspace file |
| Registry | `crate::operations::registry` | the one place executors are registered |

## Adding a tool

1. **Write an executor** implementing `OperationExecutor`. Its contract
   declares the capability actions it needs, whether it is idempotent, whether
   it is **reversible**, and its input schema. Store the result as an artifact
   and return its reference as `OperationOutcome::value`.
2. **Register it** in `crate::operations::registry`. Nothing else registers
   executors, so `arsy policy explain`, `arsy serve`, and a turn cannot end up
   with different sets.
3. **Add a `Tool`** to `agent::TOOLS` if the model should see it: a name, a
   description, a JSON Schema, and a `translate` function from the model's
   arguments to the operation's input.
4. **Render its result** in `agent::present` if the default pretty-printed
   JSON is not what the model should read.

`ToolRuntime::schemas` filters by what the registry can actually dispatch, so a
tool whose executor is not registered is never offered.

### Reversible is not the same as idempotent

`Idempotency` says whether *repeating* a call repeats its effect.
`OperationContract::reversible` says whether the effect can be undone.

Policy raises an irreversible call to approval however permissive a rule is.
Equating the two would make every edit need a human, which is how an operator
learns to say yes without reading. Rewriting a file is effectful and
reversible — version control still has the previous content. Removing a file,
or running a command, is neither.

## Authority

Policy decides every call; a front end only chooses what to do about the
answer.

| Decision | TUI | `arsy run` | `arsy serve` |
|---|---|---|---|
| `Allowed` | runs, no prompt | runs | runs |
| `NeedsApproval` | prompts, and a "yes" mints a grant over exactly the resource shown | refused, reported to the model | refused |
| `Denied` | refused, reported to the model | refused | JSON-RPC `-32020` |

The default configuration is `policy.default_effect = "ask"` on every action,
so a first run confirms everything. An operator who wants edits without prompts
writes `allow` rules for `fs.write`; deletions and shell commands still confirm,
because they are declared irreversible.

`ApprovalRequest::grant` produces a grant over the literal resource, with
delegation depth zero. An approval is never a mode.

## Context

The system prompt is rebuilt each round from:

```text
HARNESS_INSTRUCTIONS               what the tools are for
  + AGENTS.md / CLAUDE.md          root first, then each directory down to cwd
  + task context                   when a caller supplies one
```

compiled through `arsy_kernel::prompt::compile`, so ordering follows the model
family and secrets are redacted on the way out.

Discovery is deliberately narrow — `agent::instructions::INSTRUCTION_NAMES`,
on the ancestor path only, one file per directory. `README.md` and `docs/*.md`
are *not* injected; they are documentation the model reads with `search.text`
and `fs.read` when it needs them. The objective is relevant context, not
maximum context.

The transcript is trimmed to a token budget before each request by
`agent::budget::trim`. Tool results are never removed — that would break the
call/result pairing every provider requires — their bodies are replaced by a
stub naming the call and its size. The ranking comes from
`arsy_kernel::context::ContextView::select`, so there is one selector that can
explain its own omissions rather than two that might disagree.

## Repository awareness

`Workspace` owns a cap-std `Dir` for the root. Confinement is two independent
checks: a lexical one that refuses `..`, absolute paths, and prefixes, and the
syscall-level one cap-std performs, which is what catches a symlink whose name
looks local. Nothing else in the crate opens a workspace path.

`resource::walk` is the one traversal: `.gitignore` filters plus a skip of
`.arsy`, so a listing, a text search, and a file find agree about what the
workspace contains.

## What is not here yet

- Cancellation stops the loop *between* calls (`ToolRuntime::cancellation`). A
  call already running finishes, because killing it mid-write is how a file is
  left half-edited. Interrupting a long `bash` needs the deadline.
- The context budget is a constant, not a per-model window.
- `arsy_kernel::{memory, orchestration, observer, telemetry, migrate}` and
  `arsy-code::{workspace, review, benchmark, acp, remote, extension, graph,
  intelligence, lsp, syntax}` are built but not reachable from a turn. They are
  later roadmap phases; see [`31-roadmap.md`](31-roadmap.md).
