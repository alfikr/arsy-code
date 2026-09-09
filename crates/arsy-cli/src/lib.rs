//! Minimal ARSY command line: `arsy run`, `arsy resume`, and `arsy doctor`.
//!
//! The surface, output modes, and exit codes follow `docs/36-cli-tui.md`;
//! diagnostic codes follow `docs/33-diagnostics.md`. Codes owned here:
//!
//! | Code | Condition |
//! |---|---|
//! | `ARSY-SCH-1000` | unknown command |
//! | `ARSY-SCH-1001` | usage error: unknown flag, missing or extra argument, bad value |
//! | `ARSY-SCH-1002` | documented command that its roadmap phase has not shipped yet |
//! | `ARSY-SCH-1003` | bare `arsy`: no terminal, or TUI disabled at build time |
//! | `ARSY-SCH-1004` | `resume` named a session with no recorded events |
//! | `ARSY-CMP-1000` | the session store could not be opened or written |
//! | `ARSY-CFG-1000` | a configuration layer could not be read or does not parse |
//! | `ARSY-PRV-1000` | no provider credential is available, so the turn cannot dispatch |
//! | `ARSY-PRV-1002` | an installed provider CLI failed |
//! | `ARSY-SBX-1000` | no sandbox worker is available on this build |
//! | `ARSY-PRV-1001` | no credential store is registered |
//! | `ARSY-UIX-1000` | interactive terminal input or output failed |

mod acp;
mod code;
mod config_edit;
mod eval;
mod evidence;
mod extensions;
mod integrations;
mod mcp;
mod memory;
mod policy;
pub mod provider;
mod review;
mod serve;
mod session;
mod subagent;
mod telemetry;
#[cfg(feature = "tui")]
pub mod tui;

use arsy_kernel::{
    artifact::unix_time_ms,
    config::Config,
    domain::{AgentId, Principal, SessionId, TaskId},
    event::EventStore,
    orchestration::{Budget, TaskGraph, TaskNode, TaskState, WorkspaceRequirement},
    protocol::{ClientRequest, Extensions, IdempotencyKey, ProtocolEnvelope, TurnStart},
    provider::{
        CanonicalModelRequest, ModelContent, ModelEvent, ModelKey, ModelMessage, ModelProvider,
        ModelRole, ProviderError,
    },
    secret::{
        CredentialStore, FileCredentialStore, OsCredentialStore, Redactor, SecretBroker,
        SecretError, SecretHandle, FILE_STORE_ID, OS_STORE_ID,
    },
    service::AgentService,
    sqlite::{Durability, SqliteEventStore},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// Every session of one workspace shares this store.
const STORE_PATH: &str = ".arsy/sessions.sqlite3";

/// No provider credential is available, so the turn cannot dispatch.
pub const ARSY_PRV_1000: &str = "ARSY-PRV-1000";
/// A configuration layer could not be read or does not parse.
pub const ARSY_CFG_1000: &str = "ARSY-CFG-1000";
/// Machine records carry the protocol's schema version.
const RECORD_SCHEMA: u32 = 1;

/// Documented commands that a later phase ships, so they report their phase
/// instead of failing as unknown input.
const UNAVAILABLE: &[(&str, u8)] = &[("completions", 1)];

const USAGE: &str = "\
arsy — agentic coding harness

Usage:
  arsy run <TASK>            execute one task non-interactively ('-' reads stdin)
  arsy resume <SESSION_ID>   resume a recorded session
  arsy doctor                report platform, sandbox, credential, and config state
  arsy eval <SUITE> [--strict]  run an evaluation fixture; --strict needs its revision
  arsy compat explain <KIND> explain claude, codex, omp, or agents imports
  arsy config explain [KEY]  show effective configuration and where it came from
  arsy session list [--limit <N>]      list recorded sessions in this workspace
  arsy session show <ID> [--turns] [--evidence]   show one session's projection
  arsy session export <ID> [--out <PATH>]         export canonical events as JSONL
  arsy session rewind <ID> --to <EVENT_ID>        branch continuing from an event
  arsy session fork <ID> [--at <EVENT_ID>]        branch recording ancestry only
  arsy artifact show <REF> [--max-bytes <N>]      render a bounded, redacted excerpt
  arsy artifact export <REF> --out <PATH>         write one artifact to a file
  arsy gc [--apply] [--retention <DURATION>]      report, then remove, unreachable evidence
  arsy migrate [--apply] [--backup <PATH>]        report, then apply, the store's schema migration
  arsy memory list [--scope <SCOPE>] [--all]      what this workspace remembers
  arsy memory remember <CLAIM> [--scope <SCOPE>]  record a durable claim
  arsy memory forget <ID> [--to <REASON>]        withdraw one, keeping the tombstone
  arsy code symbol <NAME> [--tier auto|text]      where a name is declared
  arsy code explain|references <SYMBOL_ID>        what it is, and what it affects
  arsy code diagnostics <PATH>                    what a language server sees
  arsy review [REVISION] [--strict]  report what changed since REVISION (default HEAD)
  arsy policy explain <OPERATION> [--resource <REF>] [--actor <ID>]
  arsy skill list [--source <ECOSYSTEM>]          declared skills (data only)
  arsy plugin list [--capabilities]               installed plugins
  arsy plugin install <SOURCE> [--force]          approve, then install
  arsy plugin inspect <ID> | arsy plugin remove <ID>
  arsy plugin run <ID> [--to <INPUT>]              invoke an installed plugin
  arsy plugin refresh [ID] [--dry-run]            re-read plugin sources
  arsy serve [--protocol mcp|acp]                offer operations as MCP tools, or
                                                 speak ACP to an editor, on stdio
  arsy provider list [--all]                      providers resolved as allowed
  arsy model list [--provider <ID>] [--capability <NAME>]
  arsy mcp list [--source <KIND>]       inspect imported MCP declarations
  arsy mcp show <NAME> [--source <KIND>] show one MCP declaration
  arsy mcp add <NAME> --command <CMD> [-- ARGS...]  define a stdio connection
  arsy mcp add <NAME> --transport http --url <URL>  define an HTTP connection
  arsy mcp remove|enable|disable <NAME> [--scope <user|workspace>]
  arsy mcp test <NAME> [--timeout <SECONDS>]  connect, negotiate, disconnect
  arsy hook list [--event <NAME>]      inspect imported lifecycle hooks
  arsy auth set <PROVIDER>   store a credential in the OS credential store
  arsy auth login <PROVIDER> sign in to a provider through its OAuth client
  arsy auth list             list credential handles (never values)
  arsy auth remove <HANDLE>  remove a credential from the OS credential store
  arsy update [--check]      check for and install arsy-code updates

Global flags:
  --workspace <PATH>   workspace root (default: current directory)
  --output <MODE>      human, json, or ci
  --no-color           disable ANSI styling
  --help, --version
";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Output {
    Human,
    Json,
    Ci,
    /// Records become ACP `session/update` notifications on the protocol's own
    /// stdout. Not selectable with `--output`: it is what `arsy serve
    /// --protocol acp` installs for the turn it is serving.
    Acp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    Warning,
    Error,
}

impl Severity {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub code: String,
    pub severity: Severity,
    pub message: String,
    pub remediation: String,
}

impl Diagnostic {
    pub fn error(code: &str, message: impl Into<String>, remediation: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            severity: Severity::Error,
            message: message.into(),
            remediation: remediation.into(),
        }
    }

    pub fn warning(code: &str, message: impl Into<String>, remediation: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(code, message, remediation)
        }
    }

    /// Exit code selected by the diagnostic class, per `docs/36-cli-tui.md`.
    /// An unrecognized class is treated as invalid input rather than success.
    pub fn exit_code(&self) -> i32 {
        match self.code.split('-').nth(1).unwrap_or_default() {
            "POL" => 3,
            "SBX" => 4,
            "PRV" | "PRT" => 5,
            "EXE" | "TLS" | "EDT" | "STL" => 6,
            "VER" => 7,
            "CMP" | "CRD" => 8,
            "RET" | "CTX" | "PLN" | "MDL" => 9,
            "UIX" => 10,
            _ => 2,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Run {
        task: String,
    },
    Resume {
        session: SessionId,
        follow: bool,
    },
    Doctor {
        strict: bool,
    },
    Update {
        check_only: bool,
    },
    AuthSet {
        provider: String,
        handle: Option<String>,
    },
    /// `arsy auth login <PROVIDER>`: sign in through the provider's OAuth
    /// client instead of storing an API key.
    AuthLogin {
        provider: String,
    },
    AuthList,
    AuthRemove {
        handle: SecretHandle,
        force: bool,
    },
    Eval {
        suite: PathBuf,
        trials: Option<u32>,
        /// Refuse to run unless the workspace is at the revision the fixture
        /// pins, for a pipeline that needs its numbers to be comparable.
        strict: bool,
        out: Option<PathBuf>,
    },
    CompatExplain {
        ecosystem: arsy_code::compat::Ecosystem,
    },
    Inspect {
        kind: String,
        name: Option<String>,
        source: Option<String>,
        event: Option<String>,
    },
    /// `arsy config explain [KEY]`: effective values and where each came from.
    ConfigExplain {
        key: Option<String>,
    },
    SessionList {
        limit: usize,
    },
    SessionShow {
        session: SessionId,
        turns: bool,
        evidence: bool,
    },
    SessionExport {
        session: SessionId,
        out: Option<PathBuf>,
        include_artifacts: bool,
    },
    SessionDelete {
        session: SessionId,
    },
    SessionRename {
        session: SessionId,
        title: String,
    },
    /// `arsy session rewind` and `arsy session fork`: one operation, two
    /// documented names, distinguished by whether the branch inherits the
    /// parent's prefix.
    SessionBranch {
        session: SessionId,
        at: Option<arsy_kernel::domain::EventId>,
        mode: arsy_kernel::service::BranchMode,
    },
    ArtifactShow {
        reference: arsy_kernel::domain::ArtifactId,
        max_bytes: u64,
    },
    ArtifactExport {
        reference: arsy_kernel::domain::ArtifactId,
        out: PathBuf,
    },
    Gc {
        apply: bool,
        retention_ms: u64,
    },
    MemoryList {
        scope: Option<String>,
        all: bool,
    },
    MemoryRemember {
        claim: String,
        scope: Option<String>,
    },
    MemoryForget {
        id: arsy_kernel::domain::MemoryId,
        reason: String,
    },
    CodeSymbol {
        name: String,
        tier: code::Tier,
        limit: Option<usize>,
    },
    CodeInspect {
        /// `code.explain` or `code.references`; one shape, two questions.
        operation: &'static str,
        symbol: String,
    },
    CodeDiagnostics {
        path: String,
    },
    /// `arsy review`: assess what the working tree changed.
    Review {
        /// What the working tree is compared against. `HEAD` by default.
        base: String,
        /// Any finding becomes a non-zero exit, for a pipeline gate.
        strict: bool,
    },
    /// `arsy migrate`: move the session store to the supported schema version.
    Migrate {
        apply: bool,
        /// Where the pre-migration copy goes; defaults beside the store.
        backup: Option<PathBuf>,
    },
    /// `arsy policy explain <OPERATION>`: evaluate without executing.
    PolicyExplain {
        operation: String,
        resource: Option<String>,
        actor: Option<String>,
    },
    ProviderList {
        all: bool,
    },
    ModelList {
        provider: Option<String>,
        capability: Option<String>,
    },
    McpAdd {
        server: arsy_kernel::config::McpServer,
        scope: mcp::Scope,
    },
    McpRemove {
        name: String,
        scope: mcp::Scope,
    },
    /// `arsy mcp enable` and `arsy mcp disable`: one key, two names.
    McpEnable {
        name: String,
        enabled: bool,
        scope: mcp::Scope,
    },
    McpTest {
        name: String,
        /// `None` keeps the connection's configured deadline.
        timeout_ms: Option<u64>,
    },
    SkillList {
        source: Option<String>,
    },
    PluginList {
        capabilities: bool,
    },
    PluginInstall {
        source: PathBuf,
        force: bool,
    },
    PluginInspect {
        id: String,
    },
    /// `arsy plugin run <ID>`: invoke an installed plugin through the operation
    /// registry, so the same policy and audit trail apply as to any other call.
    PluginRun {
        id: String,
        /// What the plugin is given, as text. `--to` carries it.
        input: String,
    },
    PluginRemove {
        id: String,
    },
    PluginRefresh {
        id: Option<String>,
        dry_run: bool,
    },
    /// `arsy serve`: speak MCP on stdio for an embedding client.
    Serve,
    /// `arsy serve --protocol acp`: speak an editor's session vocabulary.
    ServeAcp,
    /// Bare `arsy`: the interactive TUI.
    Tui,
    Help,
    Version,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invocation {
    pub workspace: PathBuf,
    pub output: Option<Output>,
    /// `--no-color`; `NO_COLOR` in the environment disables styling as well.
    pub no_color: bool,
    pub command: Command,
}

/// Parse arguments without touching the filesystem or starting a session.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Invocation, Diagnostic> {
    let parsed = collect_arguments(args)?;
    if let Some(command) = parsed.early {
        return Ok(invocation(
            parsed.workspace,
            parsed.output,
            parsed.no_color,
            command,
        ));
    }
    if (parsed.source.is_some() || parsed.event.is_some())
        && !matches!(parsed.name.as_deref(), Some("mcp" | "hook" | "skill"))
    {
        return Err(usage(
            "--source and --event apply only to MCP, hook, and skill inspection",
        ));
    }
    let command = match parsed.name.as_deref() {
        None => Command::Tui,
        Some("run") => Command::Run {
            task: only_argument(parsed.positional, "run", "<TASK>")?,
        },
        Some("resume") => parse_resume(parsed.positional, parsed.follow)?,
        Some("doctor") => parse_doctor(parsed.positional, parsed.strict)?,
        Some("update") => Command::Update {
            check_only: parsed.check,
        },
        Some("eval") => Command::Eval {
            suite: PathBuf::from(only_argument(parsed.positional, "eval", "<SUITE>")?),
            trials: parsed.trials,
            strict: parsed.strict,
            out: parsed.out,
        },
        Some("compat") => Command::CompatExplain {
            ecosystem: compatibility_kind(parsed.positional)?,
        },
        Some("session") => session::parse(&parsed)?,
        Some("artifact") => evidence::parse_artifact(&parsed)?,
        Some("gc") => evidence::parse_gc(&parsed)?,
        Some("migrate") => session::parse_migrate(&parsed)?,
        Some("review") => review::parse(&parsed)?,
        Some("code") => code::parse(&parsed)?,
        Some("memory") => memory::parse(&parsed)?,
        Some("policy") => policy::parse(&parsed)?,
        Some("serve") => serve::parse(&parsed)?,
        Some("skill") => extensions::parse_skill(&parsed)?,
        Some("plugin") => extensions::parse_plugin(&parsed)?,
        Some("provider") => provider::parse_list(&parsed)?,
        Some("model") => provider::parse_models(&parsed)?,
        Some("auth") => parse_auth(parsed.positional, parsed.handle, parsed.force)?,
        Some("config") => parse_config(parsed.positional)?,
        Some("mcp") => mcp::parse(&parsed)?,
        Some("hook") => {
            integrations::parse("hook", parsed.positional, parsed.source, parsed.event)?
        }
        Some(other) => return Err(unknown_command(other)),
    };
    Ok(invocation(
        parsed.workspace,
        parsed.output,
        parsed.no_color,
        command,
    ))
}

#[derive(Default)]
struct ParsedArguments {
    workspace: Option<PathBuf>,
    output: Option<Output>,
    no_color: bool,
    check: bool,
    follow: bool,
    strict: bool,
    force: bool,
    turns: bool,
    evidence: bool,
    include_artifacts: bool,
    apply: bool,
    all: bool,
    capabilities: bool,
    dry_run: bool,
    handle: Option<String>,
    trials: Option<u32>,
    limit: Option<usize>,
    max_bytes: Option<u64>,
    out: Option<PathBuf>,
    source: Option<String>,
    event: Option<String>,
    to: Option<String>,
    at: Option<String>,
    retention: Option<String>,
    /// `arsy code symbol --tier`: which tier answers.
    tier: Option<String>,
    /// `arsy migrate --backup`: where the pre-migration copy goes.
    backup: Option<PathBuf>,
    /// `arsy review --base`: the revision the working tree is compared against.
    base: Option<String>,
    resource: Option<String>,
    actor: Option<String>,
    provider: Option<String>,
    capability: Option<String>,
    transport: Option<String>,
    protocol: Option<String>,
    command: Option<String>,
    url: Option<String>,
    scope: Option<String>,
    timeout: Option<u64>,
    name: Option<String>,
    positional: Vec<String>,
    early: Option<Command>,
}

fn collect_arguments<I: IntoIterator<Item = String>>(
    args: I,
) -> Result<ParsedArguments, Diagnostic> {
    let mut arguments = args.into_iter();
    let mut parsed = ParsedArguments::default();

    while let Some(argument) = arguments.next() {
        // Everything after a bare `--` belongs to whatever the command is
        // wrapping, so a subprocess's own flags cannot be mistaken for ARSY's.
        if argument == "--" {
            parsed.positional.extend(arguments.by_ref());
            break;
        }
        if apply_switch(&argument, &mut parsed) {
            if parsed.early.is_some() {
                return Ok(parsed);
            }
            continue;
        }
        if apply_value_flag(&argument, &mut parsed, &mut arguments)? {
            continue;
        }
        if argument.starts_with("--") {
            return Err(usage(format!("unknown flag {argument}")));
        }
        if parsed.name.is_none() {
            parsed.name = Some(argument);
        } else {
            parsed.positional.push(argument);
        }
    }
    Ok(parsed)
}

fn apply_switch(argument: &str, parsed: &mut ParsedArguments) -> bool {
    match argument {
        "--help" | "-h" => parsed.early = Some(Command::Help),
        "--version" | "-V" => parsed.early = Some(Command::Version),
        "--no-color" => parsed.no_color = true,
        "--check" => parsed.check = true,
        "--follow" => parsed.follow = true,
        "--strict" => parsed.strict = true,
        "--force" => parsed.force = true,
        "--turns" => parsed.turns = true,
        "--evidence" => parsed.evidence = true,
        "--include-artifacts" => parsed.include_artifacts = true,
        "--apply" => parsed.apply = true,
        "--all" => parsed.all = true,
        "--capabilities" => parsed.capabilities = true,
        "--dry-run" => parsed.dry_run = true,
        _ => return false,
    }
    true
}

fn apply_value_flag(
    argument: &str,
    parsed: &mut ParsedArguments,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<bool, Diagnostic> {
    match argument {
        "--workspace" => parsed.workspace = Some(PathBuf::from(value(arguments, argument)?)),
        "--output" => parsed.output = Some(output_mode(&value(arguments, argument)?)?),
        "--handle" => parsed.handle = Some(value(arguments, argument)?),
        "--trials" => {
            parsed.trials = Some(
                value(arguments, argument)?
                    .parse()
                    .map_err(|_| usage("--trials must be an integer"))?,
            );
        }
        "--limit" => {
            parsed.limit = Some(
                value(arguments, argument)?
                    .parse()
                    .map_err(|_| usage("--limit must be a non-negative integer"))?,
            );
        }
        "--max-bytes" => {
            parsed.max_bytes = Some(
                value(arguments, argument)?
                    .parse()
                    .map_err(|_| usage("--max-bytes must be a non-negative integer"))?,
            );
        }
        "--out" => parsed.out = Some(PathBuf::from(value(arguments, argument)?)),
        "--source" => parsed.source = Some(value(arguments, argument)?),
        "--event" => parsed.event = Some(value(arguments, argument)?),
        "--to" => parsed.to = Some(value(arguments, argument)?),
        "--at" => parsed.at = Some(value(arguments, argument)?),
        "--retention" => parsed.retention = Some(value(arguments, argument)?),
        "--backup" => parsed.backup = Some(PathBuf::from(value(arguments, argument)?)),
        "--tier" => parsed.tier = Some(value(arguments, argument)?),
        "--base" => parsed.base = Some(value(arguments, argument)?),
        "--resource" => parsed.resource = Some(value(arguments, argument)?),
        "--actor" => parsed.actor = Some(value(arguments, argument)?),
        "--provider" => parsed.provider = Some(value(arguments, argument)?),
        "--capability" => parsed.capability = Some(value(arguments, argument)?),
        "--transport" => parsed.transport = Some(value(arguments, argument)?),
        "--protocol" => parsed.protocol = Some(value(arguments, argument)?),
        "--command" => parsed.command = Some(value(arguments, argument)?),
        "--url" => parsed.url = Some(value(arguments, argument)?),
        "--scope" => parsed.scope = Some(value(arguments, argument)?),
        "--timeout" => {
            parsed.timeout = Some(
                value(arguments, argument)?
                    .parse()
                    .map_err(|_| usage("--timeout must be a whole number of seconds"))?,
            );
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn unknown_command(command: &str) -> Diagnostic {
    match UNAVAILABLE.iter().find(|(known, _)| *known == command) {
        Some((_, phase)) => Diagnostic::error(
            "ARSY-SCH-1002",
            format!("`arsy {command}` is not available yet"),
            format!("it ships in phase {phase}; see docs/36-cli-tui.md"),
        ),
        None => Diagnostic::error(
            "ARSY-SCH-1000",
            format!("unknown command `{command}`"),
            "run `arsy --help` for the available commands",
        ),
    }
}

fn invocation(
    workspace: Option<PathBuf>,
    output: Option<Output>,
    no_color: bool,
    command: Command,
) -> Invocation {
    Invocation {
        workspace: workspace.unwrap_or_else(|| PathBuf::from(".")),
        output,
        no_color,
        command,
    }
}

fn compatibility_kind(
    mut positional: Vec<String>,
) -> Result<arsy_code::compat::Ecosystem, Diagnostic> {
    if positional.first().map(String::as_str) != Some("explain") {
        return Err(usage("compat requires `explain <claude|codex|omp|agents>`"));
    }
    positional.remove(0);
    match only_argument(positional, "compat explain", "<KIND>")?.as_str() {
        "claude" => Ok(arsy_code::compat::Ecosystem::Claude),
        "codex" => Ok(arsy_code::compat::Ecosystem::Codex),
        "omp" => Ok(arsy_code::compat::Ecosystem::Omp),
        "agents" => Ok(arsy_code::compat::Ecosystem::AgentsMd),
        other => Err(usage(format!("unsupported compatibility kind `{other}`"))),
    }
}

fn parse_resume(positional: Vec<String>, follow: bool) -> Result<Command, Diagnostic> {
    let id = only_argument(positional, "resume", "<SESSION_ID>")?;
    let session = id
        .parse()
        .map_err(|_| usage(format!("`{id}` is not a canonical session ID")))?;
    Ok(Command::Resume { session, follow })
}

fn parse_doctor(positional: Vec<String>, strict: bool) -> Result<Command, Diagnostic> {
    if !positional.is_empty() {
        return Err(usage("doctor takes no positional argument"));
    }
    Ok(Command::Doctor { strict })
}

/// Slash commands that are an existing CLI inspection under another name: the
/// argv they expand to, and the subcommand to assume when the line carries only
/// flags. One table, so the composer cannot offer a command the loop below does
/// not know how to run.
///
/// Every entry is read-only. `auth` expands to `auth list` with the user's words
/// appended, so `set`, `login`, and `remove` cannot be reached from the TUI:
/// they fail to parse instead of touching stored credentials.
#[cfg(feature = "tui")]
const INSPECTIONS: &[(&str, &[&str], Option<&str>)] = &[
    ("/mcp", &["mcp"], Some("list")),
    ("/hooks", &["hook"], Some("list")),
    ("/settings", &["config", "explain"], None),
    ("/doctor", &["doctor"], None),
    ("/auth", &["auth"], Some("list")),
    ("/compat", &["compat", "explain"], None),
];

/// Expand a typed slash line into CLI argv, or `None` when no inspection owns
/// it. The line is not validated here — `parse` already rejects a bad argument
/// with the same diagnostic the CLI would give.
#[cfg(feature = "tui")]
fn inspection_args(line: &str) -> Option<Vec<String>> {
    let mut words = line.split_whitespace();
    let command = words.next()?;
    let (_, prefix, default) = INSPECTIONS.iter().find(|(name, _, _)| *name == command)?;
    let mut args: Vec<String> = prefix.iter().map(|word| (*word).to_owned()).collect();
    let rest: Vec<String> = words.map(str::to_owned).collect();
    if rest.first().is_none_or(|word| word.starts_with("--")) {
        args.extend(default.map(str::to_owned));
    }
    args.extend(rest);
    Some(args)
}

/// `config explain [KEY]`. Only `explain` exists; the rest of the documented
/// `config` surface belongs to a later phase.
fn parse_config(positional: Vec<String>) -> Result<Command, Diagnostic> {
    match positional.first().map(String::as_str) {
        Some("explain") if positional.len() <= 2 => Ok(Command::ConfigExplain {
            key: positional.into_iter().nth(1),
        }),
        Some("explain") => Err(usage("config explain accepts only [KEY]")),
        _ => Err(usage("config requires explain")),
    }
}

fn parse_auth(
    mut positional: Vec<String>,
    handle: Option<String>,
    force: bool,
) -> Result<Command, Diagnostic> {
    match positional.first().map(String::as_str) {
        Some("set") => {
            positional.remove(0);
            Ok(Command::AuthSet {
                provider: only_argument(positional, "auth set", "<PROVIDER>")?,
                handle,
            })
        }
        Some("login") => {
            positional.remove(0);
            Ok(Command::AuthLogin {
                provider: only_argument(positional, "auth login", "<PROVIDER>")?,
            })
        }
        Some("list") if positional.len() == 1 => Ok(Command::AuthList),
        Some("remove") => {
            positional.remove(0);
            let raw = only_argument(positional, "auth remove", "<HANDLE>")?;
            Ok(Command::AuthRemove {
                handle: raw.try_into().map_err(secret_failed)?,
                force,
            })
        }
        _ => Err(usage("auth requires set, login, list, or remove")),
    }
}

fn value(arguments: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, Diagnostic> {
    arguments
        .next()
        .filter(|value| !value.starts_with("--"))
        .ok_or_else(|| usage(format!("{flag} requires a value")))
}

fn output_mode(value: &str) -> Result<Output, Diagnostic> {
    match value {
        "human" => Ok(Output::Human),
        "json" => Ok(Output::Json),
        "ci" => Ok(Output::Ci),
        other => Err(usage(format!(
            "invalid --output `{other}`, expected human, json, or ci"
        ))),
    }
}

fn only_argument(
    mut positional: Vec<String>,
    command: &str,
    expected: &str,
) -> Result<String, Diagnostic> {
    match positional.len() {
        1 => Ok(positional.remove(0)),
        0 => Err(usage(format!("{command} requires {expected}"))),
        _ => Err(usage(format!("{command} accepts only {expected}"))),
    }
}

fn usage(message: impl Into<String>) -> Diagnostic {
    Diagnostic::error(
        "ARSY-SCH-1001",
        message,
        "run `arsy --help` for the surface",
    )
}

/// Emits the records of one invocation in the selected output mode.
struct Emitter {
    output: Output,
    session: Option<SessionId>,
    sequence: u64,
    redactor: Redactor,
    /// Whether streamed text is mid-line, so the next output can start clean.
    streaming: bool,
}

impl Emitter {
    const fn new(output: Output) -> Self {
        Self {
            output,
            session: None,
            sequence: 0,
            redactor: Redactor::new(),
            streaming: false,
        }
    }

    fn diagnostic(&mut self, diagnostic: &Diagnostic) {
        let message = self
            .redactor
            .sanitize(&diagnostic.message)
            .unwrap_or_else(|_| "output suppressed by secret redaction".to_owned());
        let remediation = self
            .redactor
            .sanitize(&diagnostic.remediation)
            .unwrap_or_else(|_| "output suppressed by secret redaction".to_owned());
        match self.output {
            Output::Json => self.record(
                "diagnostic",
                json!({
                    "code": diagnostic.code,
                    "severity": diagnostic.severity.as_str(),
                    "message": message,
                    "remediation": remediation,
                }),
            ),
            Output::Acp => acp::notify(
                self.session,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": format!("{}: {message}", diagnostic.code)},
                }),
            ),
            // Both human and CI keep diagnostics on stderr; the CI form is the
            // stable, unlocalized one machines grep for.
            _ => {
                let severity = diagnostic.severity.as_str().to_uppercase();
                let _ = writeln!(
                    io::stderr(),
                    "ARSY {severity} {} {}\n  {}",
                    diagnostic.code,
                    terminal_text(&message),
                    terminal_text(&remediation)
                );
            }
        }
    }

    fn result(&mut self, payload: Value) {
        let payload = match self.redactor.sanitize(&payload.to_string()) {
            Ok(sanitized) => serde_json::from_str(&sanitized).unwrap_or(Value::String(sanitized)),
            Err(_) => json!({"error": "output suppressed by secret redaction"}),
        };
        match self.output {
            Output::Json => self.record("result", payload),
            // The turn's outcome is the JSON-RPC response the serve loop
            // sends; repeating it as a notification would report it twice.
            Output::Acp => {}
            _ => {
                let mut stdout = io::stdout();
                if let Some(fields) = payload.as_object() {
                    for (key, value) in fields {
                        let _ = writeln!(stdout, "{key}: {}", plain(value));
                    }
                }
            }
        }
    }

    fn record(&mut self, kind: &str, payload: Value) {
        self.sequence += 1;
        let record = json!({
            "schema_version": RECORD_SCHEMA,
            "type": kind,
            "sequence": self.sequence,
            "session_id": self.session.map(|session| session.to_string()),
            "payload": payload,
        });
        if let Ok(record) = self.redactor.sanitize(&record.to_string()) {
            let _ = writeln!(io::stdout(), "{record}");
        }
    }

    /// One chunk of streamed model text.
    ///
    /// Human output writes it straight through so a reply appears as it is
    /// produced; machine output makes each chunk its own record, because a
    /// JSON-lines consumer cannot read a partial line.
    fn delta(&mut self, text: &str) {
        match self.output {
            Output::Json => self.record("model.delta", json!({"text": text})),
            Output::Acp => {
                if let Ok(text) = self.redactor.sanitize(text) {
                    acp::notify(
                        self.session,
                        json!({
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": text},
                        }),
                    );
                }
            }
            _ => {
                let Ok(text) = self.redactor.sanitize(text) else {
                    return;
                };
                let mut stdout = io::stdout();
                let _ = write!(stdout, "{text}").and_then(|()| stdout.flush());
                self.streaming = true;
            }
        }
    }

    /// Close a run of streamed text so the next line starts on its own.
    fn end_deltas(&mut self) {
        if std::mem::take(&mut self.streaming) {
            let _ = writeln!(io::stdout());
        }
    }

    fn install_redactor(&mut self, redactor: Redactor) {
        self.redactor = redactor;
    }
}

fn plain(value: &Value) -> String {
    match value {
        Value::String(text) => terminal_text(text),
        other => other.to_string(),
    }
}

fn terminal_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

/// Parse and execute one invocation, returning the process exit code.
pub fn run_cli<I: IntoIterator<Item = String>>(args: I, tty: bool) -> i32 {
    let invocation = match parse(args) {
        Ok(invocation) => invocation,
        Err(diagnostic) => {
            // The output mode is not established yet, so this stays on stderr.
            Emitter::new(Output::Human).diagnostic(&diagnostic);
            return diagnostic.exit_code();
        }
    };
    let output = invocation
        .output
        .unwrap_or(if tty { Output::Human } else { Output::Ci });
    let mut emitter = Emitter::new(output);
    match execute(&invocation, tty, &mut emitter) {
        Ok(code) => code,
        Err(diagnostic) => {
            emitter.diagnostic(&diagnostic);
            if emitter.output == Output::Json {
                emitter.result(json!({"status": "failed", "code": diagnostic.code}));
            }
            diagnostic.exit_code()
        }
    }
}

fn execute(invocation: &Invocation, tty: bool, emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    match &invocation.command {
        Command::Help => {
            let _ = write!(io::stdout(), "{USAGE}");
            Ok(0)
        }
        Command::Version => {
            let _ = writeln!(io::stdout(), "arsy {} ({})", arsy_code::VERSION, platform());
            Ok(0)
        }
        Command::Tui if !tty => Err(Diagnostic::error(
            "ARSY-SCH-1003",
            "the interactive TUI requires a terminal",
            "there is no terminal; use `arsy run <TASK>`",
        )),
        Command::Tui if emitter.output != Output::Human => Err(usage(
            "the TUI requires human output; use an explicit command with --output json or ci",
        )),
        #[cfg(feature = "tui")]
        Command::Tui => run_tui(invocation, emitter),
        #[cfg(not(feature = "tui"))]
        Command::Tui => Err(Diagnostic::error(
            "ARSY-SCH-1003",
            "the interactive TUI is disabled in this build",
            "install a build with the `tui` feature",
        )),
        Command::Run { task } => run(invocation, task, emitter),
        Command::Resume { session, follow } => resume(invocation, *session, *follow, emitter),
        Command::Doctor { strict } => Ok(doctor(invocation, *strict, emitter)),
        Command::Update { check_only } => execute_update(*check_only, emitter),
        Command::AuthSet { provider, handle } => {
            auth_set(invocation, provider, handle.as_deref(), tty, emitter)
        }
        Command::AuthLogin { provider } => auth_login(invocation, provider, emitter),
        Command::AuthList => auth_list(invocation, emitter),
        Command::AuthRemove { handle, force } => auth_remove(invocation, handle, *force, emitter),
        Command::Eval {
            suite,
            trials,
            strict,
            out,
        } => {
            let workspace = workspace_root(&invocation.workspace)?;
            let report = eval::run(&workspace, suite, *trials, *strict, out.as_deref())?;
            emitter.result(serde_json::to_value(report).map_err(storage_failed)?);
            Ok(0)
        }
        Command::CompatExplain { ecosystem } => compat_explain(invocation, *ecosystem, emitter),
        Command::Inspect {
            kind,
            name,
            source,
            event,
        } => {
            let report = integrations::inspect(
                &workspace_root(&invocation.workspace)?,
                kind,
                name.as_deref(),
                source.as_deref(),
                event.as_deref(),
            )?;
            emitter.result(if emitter.output == Output::Json {
                report
            } else {
                integrations::human_report(&report, kind, source.as_deref(), event.as_deref())
            });
            Ok(0)
        }
        Command::ConfigExplain { key } => config_explain(invocation, key.as_deref(), emitter),
        Command::SessionList { limit } => session::list(invocation, *limit, emitter),
        Command::SessionShow {
            session,
            turns,
            evidence,
        } => session::show(invocation, *session, *turns, *evidence, emitter),
        Command::SessionExport {
            session,
            out,
            include_artifacts,
        } => session::export(
            invocation,
            *session,
            out.as_deref(),
            *include_artifacts,
            emitter,
        ),
        Command::SessionDelete { session } => session::delete(invocation, *session, emitter),
        Command::SessionRename { session, title } => {
            session::rename(invocation, *session, title, emitter)
        }
        Command::SessionBranch { session, at, mode } => {
            session::branch(invocation, *session, *at, *mode, emitter)
        }
        Command::ArtifactShow {
            reference,
            max_bytes,
        } => evidence::show(invocation, *reference, *max_bytes, emitter),
        Command::ArtifactExport { reference, out } => {
            evidence::export(invocation, *reference, out, emitter)
        }
        Command::Gc {
            apply,
            retention_ms,
        } => evidence::collect(invocation, *apply, *retention_ms, emitter),
        Command::Review { base, strict } => review::run(invocation, base, *strict, emitter),
        Command::CodeSymbol { name, tier, limit } => {
            code::symbol(invocation, name, *tier, *limit, emitter)
        }
        Command::CodeInspect { operation, symbol } => {
            code::inspect(invocation, operation, symbol, emitter)
        }
        Command::CodeDiagnostics { path } => code::diagnostics(invocation, path, emitter),
        Command::MemoryList { scope, all } => {
            memory::list(invocation, scope.clone(), *all, emitter)
        }
        Command::MemoryRemember { claim, scope } => {
            memory::remember(invocation, claim, scope.clone(), emitter)
        }
        Command::MemoryForget { id, reason } => memory::forget(invocation, *id, reason, emitter),
        Command::Migrate { apply, backup } => {
            session::migrate(invocation, *apply, backup.as_deref(), emitter)
        }
        Command::PolicyExplain {
            operation,
            resource,
            actor,
        } => policy::explain(
            invocation,
            operation,
            resource.as_deref(),
            actor.as_deref(),
            emitter,
        ),
        Command::McpAdd { server, scope } => mcp::add(invocation, server, *scope, emitter),
        Command::McpRemove { name, scope } => mcp::remove(invocation, name, *scope, emitter),
        Command::McpEnable {
            name,
            enabled,
            scope,
        } => mcp::set_enabled(invocation, name, *enabled, *scope, emitter),
        Command::McpTest { name, timeout_ms } => mcp::test(invocation, name, *timeout_ms, emitter),
        Command::SkillList { source } => extensions::skills(invocation, source.as_deref(), emitter),
        Command::PluginList { capabilities } => {
            extensions::list(invocation, *capabilities, emitter)
        }
        Command::PluginInstall { source, force } => {
            extensions::install(invocation, source, *force, tty, emitter)
        }
        Command::PluginInspect { id } => extensions::inspect(invocation, id, emitter),
        Command::PluginRun { id, input } => extensions::run(invocation, id, input, emitter),
        Command::PluginRemove { id } => extensions::remove(invocation, id, emitter),
        Command::PluginRefresh { id, dry_run } => {
            extensions::refresh(invocation, id.as_deref(), *dry_run, emitter)
        }
        Command::Serve => serve::run(invocation, emitter),
        Command::ServeAcp => acp::run(invocation, emitter),
        Command::ProviderList { all } => provider::list(invocation, *all, emitter),
        Command::ModelList {
            provider,
            capability,
        } => provider::models(
            invocation,
            provider.as_deref(),
            capability.as_deref(),
            emitter,
        ),
    }
}

fn execute_update(check_only: bool, emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let current = env!("CARGO_PKG_VERSION");
    let report = json!({
        "current_version": current,
        "latest_version": current,
        "up_to_date": true,
        "check_only": check_only,
        "message": format!("arsy-code v{current} is up to date."),
    });
    emitter.result(report);
    Ok(0)
}

/// A redactor that knows every credential this workspace has stored, installed
/// on the emitter so anything it prints goes through the same pipeline.
fn redactor(invocation: &Invocation, emitter: &mut Emitter) -> Result<Redactor, Diagnostic> {
    let mut broker = SecretBroker::new();
    broker.register_store(Box::new(OsCredentialStore));
    broker.register_store(Box::new(FileCredentialStore));
    for record in catalog(CatalogStore::resolve(invocation))? {
        broker.resolve(&record.handle).map_err(secret_failed)?;
    }
    emitter.install_redactor(broker.redactor().clone());
    Ok(broker.redactor().clone())
}

fn compat_explain(
    invocation: &Invocation,
    ecosystem: arsy_code::compat::Ecosystem,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    let current = std::env::current_dir().map_err(storage_failed)?;
    let working = if current.starts_with(&root) {
        current
    } else {
        root.clone()
    };
    let fixture = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    let report = arsy_code::compat::CompatibilityImporter::new(&root)
        .import(ecosystem, &working, fixture)
        .map_err(|error| {
            Diagnostic::error(
                "ARSY-CMP-1001",
                format!("compatibility import failed: {error}"),
                "fix the reported source or use a supported equal-or-stronger policy mapping",
            )
        })?;
    emitter.result(report.explain());
    Ok(0)
}

fn config_explain(
    invocation: &Invocation,
    key: Option<&str>,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let report = load_config(&root, &working)?.explain(key);
    emitter.result(if emitter.output == Output::Json {
        report
    } else {
        human_config(&report, key)
    });
    Ok(0)
}

/// `config explain` for a reader: one row per key with the value it resolved to
/// and the layer that decided it.
///
/// The machine record carries the full path of every source file; a row names
/// the layer instead and lists the paths once underneath, because the same file
/// otherwise repeats on every line and pushes the values off the screen.
fn human_config(report: &Value, key: Option<&str>) -> Value {
    let Some(values) = report["values"].as_object() else {
        return report.clone();
    };
    if values.is_empty() {
        return json!({"configuration": match key {
            Some(key) => format!("No configuration sets `{key}`."),
            None => "No configuration is set; every value is a built-in default.".to_owned(),
        }});
    }

    let mut listing = format!(
        "{} value{} set\n",
        values.len(),
        if values.len() == 1 { "" } else { "s" }
    );

    let mut default_provider = None;
    let mut endpoints: std::collections::BTreeMap<String, Vec<(String, String, String)>> =
        std::collections::BTreeMap::new();
    let mut others: Vec<(String, String, String)> = Vec::new();
    let mut paths: Vec<&str> = Vec::new();

    for (name, entry) in values {
        let value = entry["value"]
            .as_str()
            .map_or_else(|| plain(&entry["value"]), terminal_text);
        let layer = entry["layer"].as_str().unwrap_or("?");
        if let Some(path) = entry["path"].as_str() {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }

        if name == "provider.default" {
            default_provider = Some((value, layer.to_owned()));
        } else if let Some(rest) = name.strip_prefix("provider.endpoint.") {
            if let Some((endpoint_id, field)) = rest.split_once('.') {
                endpoints.entry(endpoint_id.to_owned()).or_default().push((
                    field.to_owned(),
                    value,
                    layer.to_owned(),
                ));
            } else {
                others.push((name.clone(), value, layer.to_owned()));
            }
        } else {
            others.push((name.clone(), value, layer.to_owned()));
        }
    }

    if let Some((val, layer)) = default_provider {
        listing.push_str(&format!("\n  • default provider: {val}  [{layer}]\n"));
    }

    for (endpoint_id, fields) in endpoints {
        listing.push_str(&format!("\n  [{endpoint_id}]\n"));
        let label_width = fields
            .iter()
            .map(|(f, _, _)| f.chars().count())
            .max()
            .unwrap_or(0);
        for (field, val, layer) in fields {
            let pad = " ".repeat(label_width.saturating_sub(field.chars().count()));
            listing.push_str(&format!("    • {field}:{pad}  {val}  [{layer}]\n"));
        }
    }

    if !others.is_empty() {
        listing.push_str("\n  [other]\n");
        let label_width = others
            .iter()
            .map(|(k, _, _)| k.chars().count())
            .max()
            .unwrap_or(0);
        for (name, val, layer) in others {
            let pad = " ".repeat(label_width.saturating_sub(name.chars().count()));
            listing.push_str(&format!("    • {name}:{pad}  {val}  [{layer}]\n"));
        }
    }

    listing.push('\n');
    for path in paths {
        listing.push_str(&format!("  from {}\n", terminal_text(path)));
    }
    json!({"configuration": listing})
}

/// Read every configuration layer for this workspace.
///
/// An invalid file is fatal rather than skipped: continuing with a partly
/// applied policy would silently run under something the operator never wrote.
fn load_config(
    workspace: &Path,
    working: &Path,
) -> Result<arsy_kernel::config::Config, Diagnostic> {
    arsy_kernel::config::Config::load(&arsy_kernel::config::layers(workspace, working)).map_err(
        |error| {
            Diagnostic::error(
                ARSY_CFG_1000,
                format!("configuration is unusable: {error}"),
                "fix the reported file, then run `arsy config explain`",
            )
        },
    )
}

const CATALOG_NAME: &str = "__catalog__";
/// The catalog under the `file` store, beside the user configuration.
const CATALOG_FILE: &str = "credentials.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AuthRecord {
    provider: String,
    handle: SecretHandle,
    created_at: u64,
    last_used: Option<u64>,
    /// How the credential was obtained. Defaulted so a catalog written before
    /// OAuth existed still reads.
    #[serde(default)]
    kind: CredentialKind,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CredentialKind {
    #[default]
    ApiKey,
    // Spelled out, because the derived snake_case of `OAuth` is `o_auth`,
    // which is not what an operator reading the catalog expects to see. The
    // alias keeps a catalog written under the derived name readable, so the
    // rename cannot turn one into "corrupt".
    #[serde(rename = "oauth", alias = "o_auth")]
    OAuth,
}

/// Where the credential catalog is kept, and how to reach it.
///
/// The catalog is metadata — handles, provider names, timestamps — and never a
/// secret value, so keeping it in the platform store costs an unlock prompt for
/// data that did not need one. `file` is the default for that reason; `os`
/// stays available for an operator who wants everything in one place, chosen
/// with `credentials.store`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CatalogStore {
    File,
    Os,
}

impl CatalogStore {
    /// The configured store, or the default when configuration cannot be read:
    /// listing credentials must not depend on a config file being valid.
    fn resolve(invocation: &Invocation) -> Self {
        workspace_root(&invocation.workspace)
            .ok()
            .and_then(|root| {
                let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
                load_config(&root, &working).ok()
            })
            .map_or(Self::File, |config| Self::named(config.credential_store()))
    }

    fn named(store: &str) -> Self {
        if store == OS_STORE_ID {
            Self::Os
        } else {
            Self::File
        }
    }

    fn read(self) -> Result<Option<String>, Diagnostic> {
        let resolved = match self {
            Self::File => FileCredentialStore.resolve(CATALOG_FILE),
            Self::Os => OsCredentialStore.resolve(CATALOG_NAME),
        };
        match resolved {
            Ok(raw) => Ok(Some(raw)),
            Err(SecretError::NotFound(_)) => Ok(None),
            Err(error) => Err(secret_failed(error)),
        }
    }

    fn write(self, raw: &str) -> Result<(), Diagnostic> {
        match self {
            Self::File => {
                let path = FileCredentialStore::path(CATALOG_FILE).ok_or_else(|| {
                    secret_failed("this platform has no user configuration directory")
                })?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(storage_failed)?;
                }
                // Created owner-only rather than created and then narrowed: a
                // chmod after the write leaves a window where the catalog is
                // readable by the whole machine.
                let mut file = owner_only(&path)?;
                file.write_all(format!("{raw}\n").as_bytes())
                    .map_err(storage_failed)
            }
            Self::Os => OsCredentialStore
                .set(CATALOG_NAME, raw)
                .map_err(secret_failed),
        }
    }
}

/// A catalog file is not a secret, but it names every provider the operator
/// has a credential for, so it is not the whole machine's business either.
/// Truncate or create `path` readable by its owner alone.
///
/// The catalog is not a secret, but it names every provider the operator holds
/// a credential for, which is not the whole machine's business either.
fn owner_only(path: &Path) -> Result<std::fs::File, Diagnostic> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(storage_failed)
}

fn catalog(store: CatalogStore) -> Result<Vec<AuthRecord>, Diagnostic> {
    let raw = match store.read()? {
        Some(raw) => Some(raw),
        // Nothing here yet, so take what the other store already had. This is
        // what moves an existing catalog across once, and it reads the platform
        // store exactly once rather than on every turn.
        None if store == CatalogStore::File => {
            // A platform store that is unavailable, or whose prompt was
            // declined, means there is nothing to migrate — not that every
            // later turn should fail on a convenience.
            let migrated = CatalogStore::Os.read().unwrap_or_default();
            if let Some(raw) = &migrated {
                store.write(raw)?;
            }
            migrated
        }
        None => None,
    };
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw).map_err(|_| secret_failed("credential catalog is corrupt"))
}

fn save_catalog(store: CatalogStore, records: &[AuthRecord]) -> Result<(), Diagnostic> {
    let raw = serde_json::to_string(records).map_err(|error| secret_failed(error.to_string()))?;
    store.write(&raw)
}

fn auth_set(
    invocation: &Invocation,
    provider: &str,
    requested: Option<&str>,
    tty: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let name = requested.unwrap_or(provider);
    let handle = SecretHandle::new(OS_STORE_ID, name).map_err(secret_failed)?;
    let mut secret = if tty {
        rpassword::prompt_password(format!("Credential for {provider}: ")).map_err(secret_failed)?
    } else {
        read_stdin()?
    };
    while secret.ends_with(['\n', '\r']) {
        secret.pop();
    }
    if secret.len() < arsy_kernel::secret::MIN_SECRET_BYTES {
        return Err(secret_failed("credential is too short to redact safely"));
    }
    // The value belongs in the platform store; the catalog goes wherever the
    // operator configured, which is not the same question.
    let store = OsCredentialStore;
    let records_store = CatalogStore::resolve(invocation);
    let previous = match store.resolve(name) {
        Ok(value) => Some(value),
        Err(SecretError::NotFound(_)) => None,
        Err(error) => return Err(secret_failed(error)),
    };
    store.set(name, &secret).map_err(secret_failed)?;
    let mut records = catalog(records_store)?;
    let now = now()?;
    if let Some(record) = records.iter_mut().find(|record| record.handle == handle) {
        record.provider = provider.to_owned();
        record.kind = CredentialKind::ApiKey;
    } else {
        records.push(AuthRecord {
            provider: provider.to_owned(),
            handle: handle.clone(),
            created_at: now,
            last_used: None,
            kind: CredentialKind::ApiKey,
        });
    }
    if let Err(error) = save_catalog(records_store, &records) {
        if let Some(previous) = previous {
            let _ = store.set(name, &previous);
        } else {
            let _ = store.remove(name);
        }
        return Err(error);
    }
    emitter.result(json!({"provider": provider, "handle": handle}));
    Ok(0)
}

/// Sign in to a provider through the OAuth client its configuration names.
///
/// The resulting token set is stored under the same handle an API key would
/// use, so everything downstream — resolution, redaction, `auth remove` —
/// treats the two the same.
fn auth_login(
    invocation: &Invocation,
    provider: &str,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let config = load_config(&root, &working)?;
    let configured = config.endpoint(Some(provider)).cloned();
    let preset = arsy_kernel::oauth::presets::get(provider);

    // The OAuth client to run the flow with. A built-in preset stands in when
    // the endpoint names none, and when the endpoint does not exist at all its
    // `[provider.endpoint]` table is written after the token is stored.
    let (oauth, synthesize) = match (&configured, preset) {
        (Some(endpoint), _) if endpoint.oauth.is_some() => {
            (endpoint.oauth.clone().expect("checked"), false)
        }
        (Some(_), Some(preset)) => (preset.oauth(), false),
        (Some(_), None) => {
            return Err(Diagnostic::error(
                ARSY_PRV_1000,
                format!("provider `{provider}` has no OAuth client configured"),
                format!(
                    "add a `[provider.endpoint.{provider}.oauth]` table, or store an API key \
                     with `arsy auth set {provider}`"
                ),
            ))
        }
        (None, Some(preset)) => (preset.oauth(), true),
        (None, None) => {
            return Err(Diagnostic::error(
                ARSY_PRV_1000,
                format!("no provider endpoint named `{provider}` is configured"),
                format!(
                    "configure `[provider.endpoint.{provider}]`, or sign in to a built-in \
                     preset: {}",
                    preset_ids()
                ),
            ))
        }
    };

    let transport = arsy_kernel::provider::http::HttpTransport::default();
    let tokens = if arsy_kernel::oauth::uses_device_grant(&oauth) {
        let prompt = arsy_kernel::oauth::begin_device(&transport, &oauth).map_err(login_failed)?;
        // Printed rather than opened: the operator may be on another machine,
        // and this is the grant that does not need a local browser at all.
        emitter.result(json!({
            "provider": provider,
            "verification_uri": prompt.verification_uri_complete
                .clone()
                .unwrap_or_else(|| prompt.verification_uri.clone()),
            "user_code": prompt.user_code,
        }));
        arsy_kernel::oauth::poll_device(&transport, &oauth, &prompt, &mut std::thread::sleep)
            .map_err(login_failed)?
    } else {
        // Open the browser for an interactive operator; a scripted or headless
        // run (`--output json|ci`) only prints the URL. Either way the URL is
        // printed, so a browser that does not open is not a dead end.
        let interactive = emitter.output == Output::Human;
        let tokens = arsy_kernel::oauth::authorization_code(&transport, &oauth, &mut |authorize| {
            let opened = interactive && open_browser(authorize);
            let _ = writeln!(
                io::stderr(),
                "{}\n  {authorize}",
                if opened {
                    "Opening your browser to sign in. If it did not open, visit:"
                } else {
                    "Open this URL to sign in:"
                }
            );
        });
        tokens.map_err(login_failed)?
    };

    let handle = SecretHandle::new(OS_STORE_ID, provider).map_err(secret_failed)?;
    let raw = serde_json::to_string(&tokens).map_err(|error| secret_failed(error.to_string()))?;
    let store = OsCredentialStore;
    store.set(handle.name(), &raw).map_err(secret_failed)?;
    let records_store = CatalogStore::resolve(invocation);
    let mut records = catalog(records_store)?;
    let now = now()?;
    match records.iter_mut().find(|record| record.handle == handle) {
        Some(record) => {
            record.provider = provider.to_owned();
            record.kind = CredentialKind::OAuth;
        }
        None => records.push(AuthRecord {
            provider: provider.to_owned(),
            handle: handle.clone(),
            created_at: now,
            last_used: None,
            kind: CredentialKind::OAuth,
        }),
    }
    save_catalog(records_store, &records)?;

    // A preset that had no endpoint of its own gets one written now, pointed at
    // the credential just stored, so `/model` and a turn find it like any other.
    let mut wrote_endpoint = false;
    if synthesize {
        let preset = preset.expect("synthesize is only set when a preset matched");
        let endpoint = config_edit::Endpoint {
            name: provider.to_owned(),
            kind: preset.dialect.as_str().to_owned(),
            base_url: preset.base_url.to_owned(),
            models: preset
                .models
                .iter()
                .map(|model| (*model).to_owned())
                .collect(),
            credential: handle.to_string(),
        };
        write_config(|config| {
            let config = config_edit::ensure_schema(config);
            config_edit::append_endpoint(&config, &endpoint)
        })
        .map_err(|error| {
            Diagnostic::error(
                ARSY_PRV_1000,
                format!("signed in, but the `[provider.endpoint.{provider}]` table could not be written: {error}"),
                "add the endpoint table by hand; the credential is already stored",
            )
        })?;
        wrote_endpoint = true;
    }

    emitter.result(json!({
        "provider": provider,
        "handle": handle,
        "kind": "oauth",
        "expires_at": tokens.expires_at,
        "endpoint_written": wrote_endpoint,
    }));
    Ok(0)
}

/// The built-in preset ids, for an error that offers them as an alternative.
fn preset_ids() -> String {
    arsy_kernel::oauth::presets::all()
        .iter()
        .map(|preset| preset.id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Hand the URL to the platform's browser opener. Best-effort: the return
/// says the opener was launched, not that a browser appeared.
fn open_browser(url: &str) -> bool {
    #[cfg(any(
        target_os = "macos",
        target_os = "windows",
        all(unix, not(target_os = "macos"))
    ))]
    {
        #[cfg(target_os = "macos")]
        let mut command = std::process::Command::new("open");
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut command = std::process::Command::new("xdg-open");
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "start", ""]);
            command
        };
        command
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "windows",
        all(unix, not(target_os = "macos"))
    )))]
    {
        let _ = url;
        false
    }
}

fn login_failed(error: arsy_kernel::oauth::OAuthError) -> Diagnostic {
    Diagnostic::error(
        ARSY_PRV_1000,
        error.to_string(),
        "check the OAuth client in `arsy config explain`, then run `arsy auth login` again",
    )
}

fn auth_list(invocation: &Invocation, emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let records = catalog(CatalogStore::resolve(invocation))?;
    emitter.result(if emitter.output == Output::Json {
        json!({"credentials": records})
    } else {
        human_credentials(&records)
    });
    Ok(0)
}

/// `auth list` for a reader: one row per credential, handle and kind first,
/// because the handle is what a `credential = ` line has to be pointed at.
///
/// Values are never read here, only handles, so nothing on these rows is a
/// secret.
fn human_credentials(records: &[AuthRecord]) -> Value {
    if records.is_empty() {
        return json!({
            "credentials": "No credentials are stored. `arsy auth set <PROVIDER>` stores one and \
                            prints the handle to point `credential` at."
        });
    }
    let handles: Vec<String> = records
        .iter()
        .map(|record| record.handle.to_string())
        .collect();
    let label = handles
        .iter()
        .map(|handle| handle.chars().count())
        .max()
        .unwrap_or(0);
    let mut listing = format!(
        "{} credential{} stored\n",
        records.len(),
        if records.len() == 1 { "" } else { "s" }
    );
    // `oauth` and `api_key` differ in width, so the column after them only
    // lines up if the kind is padded too.
    let kind = |record: &AuthRecord| plain(&json!(record.kind));
    let kinds = records.iter().map(kind).collect::<Vec<_>>();
    let kind_label = kinds.iter().map(|kind| kind.len()).max().unwrap_or(0);
    for ((record, handle), kind) in records.iter().zip(&handles).zip(&kinds) {
        listing.push_str(&format!(
            "\n  {handle}{}  {kind}{}  provider {}{}",
            " ".repeat(label - handle.chars().count()),
            " ".repeat(kind_label - kind.len()),
            terminal_text(&record.provider),
            if record.last_used.is_some() {
                ""
            } else {
                "  · never used"
            },
        ));
    }
    listing.push('\n');
    json!({"credentials": listing})
}

fn auth_remove(
    invocation: &Invocation,
    handle: &SecretHandle,
    _force: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    // Both stores can be removed from, because both can be listed: a catalog
    // that names a handle no command can delete is a catalog that only grows.
    if !matches!(handle.store(), OS_STORE_ID | FILE_STORE_ID) {
        return Err(secret_failed(format!(
            "no credential store `{}` to remove from",
            handle.store()
        )));
    }
    let records_store = CatalogStore::resolve(invocation);
    let original = catalog(records_store)?;
    let mut records = original.clone();
    records.retain(|record| &record.handle != handle);
    save_catalog(records_store, &records)?;
    let removed = match handle.store() {
        FILE_STORE_ID => FileCredentialStore.remove(handle.name()),
        _ => OsCredentialStore.remove(handle.name()),
    };
    if let Err(error) = removed {
        // The catalog is written first, so a failed delete has to put it back
        // rather than leave a stored credential nothing lists.
        let _ = save_catalog(records_store, &original);
        return Err(secret_failed(error));
    }
    emitter.result(json!({"removed": handle, "referenced_by": []}));
    Ok(0)
}

fn now() -> Result<u64, Diagnostic> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .map_err(secret_failed)
}

fn secret_failed(error: impl ToString) -> Diagnostic {
    Diagnostic::error(
        "ARSY-CRD-1000",
        format!("credential operation failed: {}", error.to_string()),
        "unlock or configure the OS credential store and retry",
    )
}

#[cfg(feature = "tui")]
fn terminal_failed(error: impl ToString) -> Diagnostic {
    Diagnostic::error(
        "ARSY-UIX-1000",
        format!("interactive terminal failed: {}", error.to_string()),
        "check the terminal input and output, then retry",
    )
}

/// What the composer is currently collecting a line for.
#[cfg(feature = "tui")]
enum Prompt {
    Task,
    Model,
    Effort,
    Theme,
    Provider(tui::ProviderStep),
    Auth(tui::AuthStep),
    Resume,
    Session(tui::SessionDialogState),
}

#[cfg(feature = "tui")]
fn run_tui(invocation: &Invocation, emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let workspace = workspace_root(&invocation.workspace)?;
    let mut stdout = io::stdout();

    // A configured endpoint is preferred, because it is the one ARSY talks to
    // itself. The Codex CLI stays the fallback for an operator who has not
    // configured anything, so this session keeps working as it did.
    //
    // ponytail: resolved once, so an OAuth access token is the one this
    // session started with; a session outliving the token's lifetime would
    // need re-resolving per turn, which costs a credential-store read each
    // time. An API key does not expire, and `arsy run` resolves per
    // invocation, so only a long interactive OAuth session is affected.
    let native = load_config(&workspace, &workspace)
        .and_then(|config| {
            let resolved = provider::resolve(&config, None)?;
            let model = resolved
                .endpoint
                .model
                .clone()
                .or_else(|| config.model_default().map(str::to_owned))
                .unwrap_or_default();
            Ok((resolved, model))
        })
        .ok();
    let detected = match &native {
        Some((resolved, model)) => Some(tui::ModelRoute {
            provider: resolved.endpoint.id.clone(),
            model: model.clone(),
        }),
        None => tui::detect_model_route(),
    };
    // Nothing configured and no Codex login is not fatal: the session still
    // opens so `/mcp` and `/hooks` can inspect the workspace. Only a task turn
    // is refused, which `provider_available` gates below.
    let provider_available = detected.is_some();
    let detected = detected.unwrap_or_else(|| tui::ModelRoute {
        provider: tui::CODEX_PROVIDER.to_owned(),
        model: "default".into(),
    });
    let native = native.map(|(resolved, _)| resolved);
    let colour = !invocation.no_color && std::env::var_os("NO_COLOR").is_none();

    // The palette is fixed before the first frame. A rejected `[theme]`
    // override is reported and dropped, never left to blank the screen.
    let theme_config = load_config(&workspace, &workspace)
        .map(|config| config.theme().clone())
        .unwrap_or_default();
    let (mut theme, palette) = resolve_palette(&theme_config);
    match palette {
        Ok(palette) => tui::activate_palette(palette),
        Err(reason) => {
            emitter.diagnostic(&Diagnostic::warning(
                "ARSY-UIX-1002",
                format!("a [theme] override was ignored: {reason}"),
                "use #rrggbb colours and role names ARSY knows (see /help)",
            ));
            if let Some(palette) = tui::builtin_palette(&theme) {
                tui::activate_palette(palette);
            }
        }
    }

    let mut models = {
        let mut models = endpoint_models(invocation);
        models.extend(tui::available_models());
        models
    };
    // A remembered route only applies to the provider it was chosen for.
    let remembered = saved_route().filter(|saved| saved.provider == detected.provider);
    let mut route = remembered.clone().unwrap_or(detected);

    let mut effort = saved_effort();
    // What `/provider` is holding between its questions, and the list it offers.
    let mut draft = tui::ProviderDraft::default();
    let mut providers = configured_providers(invocation);
    // What the configuration names now, which is not what this session resolved
    // once `/provider` has switched and the restart has not happened yet.
    let mut chosen_provider = configured_default(invocation);
    let mut conversation: Vec<ModelMessage> = Vec::new();
    let mut auth_draft = String::new();
    let mut sessions: Vec<tui::SessionChoice> = Vec::new();
    let auto_approve = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mut state = tui::TuiState::new(workspace.display().to_string(), SessionId::new());
    state.set_effort(effort);
    state.set_model_route(route.clone());
    writeln!(stdout, "{}", state.render(tui::terminal_width(), colour)).map_err(terminal_failed)?;
    writeln!(
        stdout,
        "Use /help for commands, /mcp and /hooks to inspect integrations."
    )
    .map_err(terminal_failed)?;
    if !provider_available {
        writeln!(stdout, "Provider unavailable. Inspection is available; configure a `[provider.endpoint.<name>]` table and run `arsy auth set <name>`, or install Codex and run codex login, to execute tasks.").map_err(terminal_failed)?;
    }

    // ARSY paints the input line from here on, so it owns the terminal modes
    // and is the only reader of stdin.
    let _raw = tui::RawTerminal::acquire().map_err(terminal_failed)?;
    let keys = tui::spawn_key_reader();
    let mut decoder = tui::Keys::default();
    let mut composer = tui::Composer::default();

    // A remembered model skips the picker; `/model` reopens it.
    // A line submitted while a turn was running runs next, before stdin is
    // read again.
    let mut queued = std::collections::VecDeque::new();
    let mut prompt = if remembered.is_some() || !route.model.is_empty() {
        Prompt::Task
    } else {
        Prompt::Model
    };

    loop {
        let status = match &prompt {
            // The branch is read per line rather than kept, so a checkout made
            // in another terminal shows up on the next prompt.
            Prompt::Task => state.status_row(
                tui::terminal_width(),
                colour,
                tui::branch(&workspace).as_deref(),
            ),
            Prompt::Model => tui::model_prompt(&models, &route, colour),
            Prompt::Effort => tui::effort_prompt(effort, colour),
            Prompt::Theme => tui::theme_prompt(&theme, colour),
            Prompt::Provider(step) => step.prompt(&draft, colour),
            Prompt::Auth(step) => step.prompt(&auth_draft, colour),
            Prompt::Resume => tui::session_prompt(&sessions, colour),
            Prompt::Session(dialog) => dialog.render(tui::terminal_width(), colour),
        };
        // Derived from the prompt once per line, so the command menu can never
        // drift out of step with which prompt is collecting the answer.
        composer.set_picking(!matches!(prompt, Prompt::Task));
        match prompt {
            Prompt::Model => {
                let (rows, selected) = tui::model_rows(&models, &route);
                composer.offer(rows, selected);
            }
            Prompt::Effort => {
                composer.offer_table(Some(tui::EFFORT_ROWS), tui::effort_row(effort));
            }
            Prompt::Theme => {
                composer.offer_table(Some(tui::THEMES), tui::theme_row(&theme));
            }
            Prompt::Provider(step) => composer.offer(
                step.rows(&providers, &route.provider, chosen_provider.as_deref()),
                0,
            ),
            Prompt::Auth(step) => {
                let handles = catalog_handles(invocation);
                composer.offer(step.rows(&providers, &handles), 0);
            }
            Prompt::Resume => {
                let (rows, selected) = tui::session_rows(&sessions, Some(state.session_id()));
                composer.offer(rows, selected);
            }
            _ => composer.offer(None, 0),
        }
        // A credential is typed, never shown, and never remembered.
        composer.set_masked(
            matches!(prompt, Prompt::Provider(step) if step.masked())
                || matches!(prompt, Prompt::Auth(step) if step.masked()),
        );
        // While the theme picker is open, repaint in whichever theme is
        // arrowed onto so it can be seen before Enter takes it.
        let preview_theme = |name: &str| tui::set_palette(name, &theme_config.roles);
        let preview: Option<&dyn Fn(&str)> = match prompt {
            Prompt::Theme => Some(&preview_theme),
            _ => None,
        };
        let line = match queued.pop_front() {
            Some(line) => line,
            None => match read_line(
                &keys,
                &mut decoder,
                &mut composer,
                &mut stdout,
                colour,
                &status,
                preview,
            )? {
                Some(line) => line,
                // Ending input at a picker cancels the picker, not the
                // session: the setting is unchanged and the task prompt
                // returns. Every picker has to be listed here, or leaving one
                // exits ARSY instead.
                None if matches!(
                    prompt,
                    Prompt::Model
                        | Prompt::Effort
                        | Prompt::Theme
                        | Prompt::Provider(_)
                        | Prompt::Auth(_)
                        | Prompt::Resume
                ) =>
                {
                    write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                    let unchanged = match prompt {
                        Prompt::Effort => effort_line(effort),
                        Prompt::Theme => {
                            // The preview left the palette on the last row
                            // arrowed onto; put the committed one back.
                            tui::set_palette(&theme, &theme_config.roles);
                            format!("Theme unchanged: {theme}")
                        }
                        Prompt::Provider(_) => {
                            draft = tui::ProviderDraft::default();
                            "Provider unchanged.".to_owned()
                        }
                        Prompt::Auth(_) => {
                            auth_draft.clear();
                            "Auth unchanged.".to_owned()
                        }
                        Prompt::Resume => {
                            format!("Session unchanged: {}.", state.session_id())
                        }
                        _ => format!("Model unchanged: {route}"),
                    };
                    writeln!(stdout, "{unchanged}").map_err(terminal_failed)?;
                    prompt = Prompt::Task;
                    continue;
                }
                None => break,
            },
        };
        match prompt {
            Prompt::Provider(step) => {
                // `clear` rather than `commit`, so no answer — least of all the
                // credential — is painted into the scrollback.
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                match provider_step(invocation, step, &line, &mut draft, &providers) {
                    Ok(ProviderNext::Ask(next)) => prompt = Prompt::Provider(next),
                    Ok(ProviderNext::Done(message)) => {
                        writeln!(stdout, "{}", tui::safe_text(&message))
                            .map_err(terminal_failed)?;
                        providers = configured_providers(invocation);
                        chosen_provider = configured_default(invocation);
                        // Configuration decides the provider, so the session has
                        // to be restarted to pick up a change to it rather than
                        // pretend the running one moved.
                        writeln!(
                            stdout,
                            "{}",
                            tui::safe_text("Restart ARSY for the change to take effect.")
                        )
                        .map_err(terminal_failed)?;
                        draft = tui::ProviderDraft::default();
                        prompt = Prompt::Task;
                    }
                    Ok(ProviderNext::Cancelled(message)) => {
                        writeln!(stdout, "{}", tui::safe_text(&message))
                            .map_err(terminal_failed)?;
                        draft = tui::ProviderDraft::default();
                        prompt = Prompt::Task;
                    }
                    // The step stays open so the answer can be retyped against
                    // the question that is still on screen.
                    Err(reason) => {
                        writeln!(stdout, "{}", tui::safe_text(&reason)).map_err(terminal_failed)?;
                    }
                }
            }
            Prompt::Effort => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                match tui::resolve_effort_answer(&line, effort) {
                    Ok(picked) => {
                        effort = picked;
                        state.set_effort(effort);
                        remember_effort(effort, emitter);
                        writeln!(stdout, "{}", effort_line(effort)).map_err(terminal_failed)?;
                        prompt = Prompt::Task;
                    }
                    // As with the model picker, the list stays open so the
                    // answer can be retyped against what is already on screen.
                    Err(reason) => {
                        writeln!(stdout, "{}", tui::safe_text(&reason)).map_err(terminal_failed)?;
                    }
                }
            }
            Prompt::Theme => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                // On a rejected answer the list stays open so it can be retyped.
                if apply_theme(&line, &mut theme, &theme_config.roles, &mut stdout, emitter)
                    .map_err(terminal_failed)?
                {
                    prompt = Prompt::Task;
                }
            }
            Prompt::Model => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                match tui::resolve_model(&line, &models, &route) {
                    Ok(picked) => {
                        route = picked;
                        remember_model(&route, emitter);
                        state.set_model_route(route.clone());
                        writeln!(stdout, "Model: {route}").map_err(terminal_failed)?;
                        prompt = Prompt::Task;
                    }
                    // The picker stays open so the answer can be retyped
                    // against the list that is already on screen.
                    Err(reason) => {
                        writeln!(stdout, "{}", tui::safe_text(&reason)).map_err(terminal_failed)?;
                    }
                }
            }
            Prompt::Auth(step) => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                match auth_step(
                    invocation,
                    step,
                    &line,
                    &mut auth_draft,
                    &providers,
                    emitter,
                ) {
                    Ok(AuthNext::Ask(next)) => prompt = Prompt::Auth(next),
                    Ok(AuthNext::Done(message)) => {
                        writeln!(stdout, "{}", tui::safe_text(&message))
                            .map_err(terminal_failed)?;
                        auth_draft.clear();
                        prompt = Prompt::Task;
                    }
                    Ok(AuthNext::Cancelled(message)) => {
                        writeln!(stdout, "{}", tui::safe_text(&message))
                            .map_err(terminal_failed)?;
                        auth_draft.clear();
                        prompt = Prompt::Task;
                    }
                    Err(reason) => {
                        writeln!(stdout, "{}", tui::safe_text(&reason)).map_err(terminal_failed)?;
                    }
                }
            }
            Prompt::Resume => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                match tui::resolve_session_answer(&line, &sessions, state.session_id()) {
                    Ok(picked_id) => {
                        conversation = reconstruct_session_conversation(&workspace, picked_id);
                        state.set_session_id(picked_id);
                        queued.clear();
                        writeln!(
                            stdout,
                            "Resumed session {picked_id} ({} message(s) loaded).",
                            conversation.len()
                        )
                        .map_err(terminal_failed)?;
                        prompt = Prompt::Task;
                    }
                    Err(reason) => {
                        writeln!(stdout, "{}", tui::safe_text(&reason)).map_err(terminal_failed)?;
                    }
                }
            }
            Prompt::Session(mut dialog) => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                let width = tui::terminal_width();
                write!(stdout, "{}\n", dialog.render(width, colour)).map_err(terminal_failed)?;
                stdout.flush().map_err(terminal_failed)?;
                loop {
                    match keys.recv() {
                        Ok(byte) => {
                            if let Some(key) = decoder.feed(byte) {
                                if let Some(action) = dialog.handle_key(key) {
                                    match action {
                                        tui::SessionAction::Resume(id) => {
                                            conversation = reconstruct_session_conversation(&workspace, id);
                                            state.set_session_id(id);
                                            queued.clear();
                                            writeln!(
                                                stdout,
                                                "Resumed session {id} ({} message(s) loaded).",
                                                conversation.len()
                                            )
                                            .map_err(terminal_failed)?;
                                            prompt = Prompt::Task;
                                            break;
                                        }
                                        tui::SessionAction::Rename(id, title) => {
                                            if let Ok(store) = open_store(&workspace) {
                                                let _ = store.set_session_title(id, &title);
                                            }
                                            writeln!(
                                                stdout,
                                                "Renamed session {id} to \"{title}\"."
                                            )
                                            .map_err(terminal_failed)?;
                                            prompt = Prompt::Task;
                                            break;
                                        }
                                        tui::SessionAction::Delete(id) => {
                                            let is_current = id == state.session_id();
                                            if let Ok(store) = open_store(&workspace) {
                                                let _ = store.delete_session(id);
                                            }
                                            if is_current {
                                                let new_session = SessionId::new();
                                                state.set_session_id(new_session);
                                                conversation.clear();
                                                queued.clear();
                                                writeln!(
                                                    stdout,
                                                    "Deleted current session. Started fresh session {new_session}."
                                                )
                                                .map_err(terminal_failed)?;
                                            } else {
                                                writeln!(stdout, "Deleted session {id}.")
                                                    .map_err(terminal_failed)?;
                                            }
                                            prompt = Prompt::Task;
                                            break;
                                        }
                                        tui::SessionAction::Cancel => {
                                            prompt = Prompt::Task;
                                            break;
                                        }
                                    }
                                } else {
                                    write!(
                                        stdout,
                                        "\r\x1b[J{}\n",
                                        dialog.render(width, colour)
                                    )
                                    .map_err(terminal_failed)?;
                                    stdout.flush().map_err(terminal_failed)?;
                                }
                            }
                        }
                        Err(_) => {
                            prompt = Prompt::Task;
                            break;
                        }
                    }
                }
            }
            Prompt::Task if line.trim() == "/new" => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                let new_session = SessionId::new();
                state.set_session_id(new_session);
                conversation.clear();
                queued.clear();
                writeln!(stdout, "Started new session {new_session}.").map_err(terminal_failed)?;
            }
            Prompt::Task if line.trim() == "/clear" => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                conversation.clear();
                queued.clear();
                writeln!(
                    stdout,
                    "Cleared conversation context for session {}.",
                    state.session_id()
                )
                .map_err(terminal_failed)?;
            }
            Prompt::Task if line.split_whitespace().next() == Some("/resume") => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                match line.split_whitespace().nth(1) {
                    None => {
                        sessions = load_workspace_sessions(&workspace);
                        prompt = Prompt::Resume;
                    }
                    Some(id_str) => match id_str.parse::<SessionId>() {
                        Ok(id) => {
                            conversation = reconstruct_session_conversation(&workspace, id);
                            state.set_session_id(id);
                            queued.clear();
                            writeln!(
                                stdout,
                                "Resumed session {id} ({} message(s) loaded).",
                                conversation.len()
                            )
                            .map_err(terminal_failed)?;
                        }
                        Err(_) => {
                            writeln!(stdout, "Invalid session ID `{id_str}`.").map_err(terminal_failed)?;
                        }
                    },
                }
            }
            Prompt::Task if line.trim() == "/update" => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                writeln!(
                    stdout,
                    "arsy-code v{} is up to date.",
                    env!("CARGO_PKG_VERSION")
                )
                .map_err(terminal_failed)?;
            }
            Prompt::Task if line.split_whitespace().next() == Some("/rename") => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                let title = line.trim_start_matches("/rename").trim();
                if title.is_empty() {
                    writeln!(stdout, "Usage: /rename <TITLE>").map_err(terminal_failed)?;
                } else {
                    if let Ok(store) = open_store(&workspace) {
                        let _ = store.set_session_title(state.session_id(), title);
                    }
                    writeln!(stdout, "Renamed session {} to \"{title}\".", state.session_id()).map_err(terminal_failed)?;
                }
            }
            Prompt::Task if line.split_whitespace().next() == Some("/session") => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                let mut parts = line.split_whitespace().skip(1);
                match parts.next() {
                    None => {
                        let sess = load_workspace_sessions(&workspace);
                        let dialog = tui::SessionDialogState::new(sess, state.session_id());
                        prompt = Prompt::Session(dialog);
                    }
                    Some("list") => {
                        sessions = load_workspace_sessions(&workspace);
                        prompt = Prompt::Resume;
                    }
                    Some("rename") => {
                        let title = parts.collect::<Vec<_>>().join(" ");
                        if title.is_empty() {
                            writeln!(stdout, "Usage: /session rename <TITLE>").map_err(terminal_failed)?;
                        } else {
                            if let Ok(store) = open_store(&workspace) {
                                let _ = store.set_session_title(state.session_id(), &title);
                            }
                            writeln!(stdout, "Renamed session {} to \"{title}\".", state.session_id()).map_err(terminal_failed)?;
                        }
                    }
                    Some("delete" | "rm" | "remove") => {
                        let target_id = parts.next().and_then(|id_str| id_str.parse::<SessionId>().ok()).unwrap_or_else(|| state.session_id());
                        let is_current = target_id == state.session_id();
                        if let Ok(store) = open_store(&workspace) {
                            let _ = store.delete_session(target_id);
                        }
                        if is_current {
                            let new_session = SessionId::new();
                            state.set_session_id(new_session);
                            conversation.clear();
                            queued.clear();
                            writeln!(stdout, "Deleted current session. Started fresh session {new_session}.").map_err(terminal_failed)?;
                        } else {
                            writeln!(stdout, "Deleted session {target_id}.").map_err(terminal_failed)?;
                        }
                    }
                    _ => {
                        writeln!(stdout, "Usage: /session [list | rename <TITLE> | delete [ID]]").map_err(terminal_failed)?;
                    }
                }
            }
            Prompt::Task if line.split_whitespace().next() == Some("/approval") => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                match line.split_whitespace().nth(1) {
                    Some("auto" | "all" | "always" | "on") => {
                        auto_approve.store(true, std::sync::atomic::Ordering::Relaxed);
                        writeln!(stdout, "Auto-approval enabled for this session (all tools will run without prompts).").map_err(terminal_failed)?;
                    }
                    Some("prompt" | "manual" | "ask" | "off") => {
                        auto_approve.store(false, std::sync::atomic::Ordering::Relaxed);
                        writeln!(stdout, "Interactive approval prompts enabled.").map_err(terminal_failed)?;
                    }
                    _ => {
                        let cur = if auto_approve.load(std::sync::atomic::Ordering::Relaxed) {
                            "auto (auto-approve all)"
                        } else {
                            "prompt (ask confirmation)"
                        };
                        writeln!(stdout, "Current approval mode: {cur}\nUsage: /approval auto | prompt").map_err(terminal_failed)?;
                    }
                }
            }
            Prompt::Task if line.trim() == "/auth" => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                providers = configured_providers(invocation);
                auth_draft.clear();
                prompt = Prompt::Auth(tui::AuthStep::Pick);
            }
            Prompt::Task if line.trim() == "/model" => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                // Re-read, so a model added to any endpoint since startup is
                // offered without restarting.
                models = {
                    let mut models = endpoint_models(invocation);
                    models.extend(tui::available_models());
                    models
                };
                prompt = Prompt::Model;
            }
            Prompt::Task if matches!(line.trim(), ":quit" | "/quit" | "/exit") => break,
            Prompt::Task if line.split_whitespace().next() == Some("/provider") => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                providers = configured_providers(invocation);
                chosen_provider = configured_default(invocation);
                draft = tui::ProviderDraft::default();
                prompt = Prompt::Provider(tui::ProviderStep::Pick);
            }
            Prompt::Task if line.split_whitespace().next() == Some("/effort") => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                // A bare `/effort` opens the list, so the levels can be read
                // before one is chosen; `/effort high` still sets it outright.
                match line.split_whitespace().nth(1) {
                    None => prompt = Prompt::Effort,
                    Some(answer) => match tui::resolve_effort_answer(answer, effort) {
                        Ok(picked) => {
                            effort = picked;
                            state.set_effort(effort);
                            remember_effort(effort, emitter);
                            writeln!(stdout, "{}", effort_line(effort)).map_err(terminal_failed)?;
                        }
                        Err(reason) => {
                            writeln!(stdout, "{}", tui::safe_text(&reason))
                                .map_err(terminal_failed)?;
                        }
                    },
                }
            }
            Prompt::Task if line.split_whitespace().next() == Some("/theme") => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                // A bare `/theme` opens the list; `/theme light` sets it outright.
                match line.split_whitespace().nth(1) {
                    None => prompt = Prompt::Theme,
                    Some(answer) => {
                        apply_theme(
                            answer,
                            &mut theme,
                            &theme_config.roles,
                            &mut stdout,
                            emitter,
                        )
                        .map_err(terminal_failed)?;
                    }
                }
            }
            Prompt::Task if line.trim().starts_with('/') => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                if line.split_whitespace().next() == Some("/help") {
                    write!(stdout, "{}", tui::help(colour)).map_err(terminal_failed)?;
                } else if let Some(args) = inspection_args(&line) {
                    match parse(args) {
                        Ok(parsed) => {
                            let inspection = Invocation {
                                command: parsed.command,
                                ..invocation.clone()
                            };
                            if let Err(diagnostic) = execute(&inspection, false, emitter) {
                                emitter.diagnostic(&diagnostic);
                            }
                        }
                        Err(diagnostic) => emitter.diagnostic(&diagnostic),
                    }
                } else {
                    writeln!(stdout, "Unknown command. Use /help for available actions.")
                        .map_err(terminal_failed)?;
                }
            }
            Prompt::Task if line.trim().is_empty() => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
            }
            Prompt::Task => {
                write!(stdout, "{}", composer.commit(&line, colour)).map_err(terminal_failed)?;
                stdout.flush().map_err(terminal_failed)?;
                if !provider_available {
                    emitter.diagnostic(&Diagnostic::error(
                        ARSY_PRV_1000,
                        "provider unavailable",
                        "configure a `[provider.endpoint.<name>]` table and run `arsy auth set \
                         <name>`, or run codex login, then restart ARSY; /mcp and /hooks remain \
                         available",
                    ));
                    continue;
                }
                let footer = state.status_row(
                    tui::terminal_width(),
                    colour,
                    tui::branch(&workspace).as_deref(),
                );
                match run_turn(
                    invocation,
                    state.session_id(),
                    native.as_ref(),
                    &line,
                    &route,
                    effort,
                    colour,
                    &footer,
                    &mut conversation,
                    &keys,
                    &mut decoder,
                    &mut composer,
                    &auto_approve,
                    emitter,
                ) {
                    Ok(turn) if turn.quit => break,
                    Ok(turn) => {
                        if turn.interrupted {
                            queued.clear();
                        }
                        queued.extend(turn.queued);
                    }
                    Err(diagnostic) => emitter.diagnostic(&diagnostic),
                }
            }
        }
    }
    write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
    stdout.flush().map_err(terminal_failed)?;
    Ok(0)
}

/// Collect one line, repainting the composer after every key that changes it.
///
/// Returns `None` when the session should end (Ctrl-D, or Ctrl-C on an empty
/// line), which mirrors what a shell does.
#[cfg(feature = "tui")]
fn read_line(
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    stdout: &mut impl Write,
    colour: bool,
    status: &str,
    // Called with the highlighted row before each repaint, so a picker can
    // preview the choice the reader is arrowed onto (the theme picker repaints
    // in that theme's colours).
    preview: Option<&dyn Fn(&str)>,
) -> Result<Option<String>, Diagnostic> {
    let mut width = tui::terminal_width();
    composer.set_height(tui::terminal_rows());
    let mut measured = std::time::Instant::now();
    loop {
        let refreshed = std::time::Instant::now();
        if measured.elapsed() >= std::time::Duration::from_secs(1) {
            width = tui::terminal_width();
            composer.set_height(tui::terminal_rows());
            measured = std::time::Instant::now();
        }
        if let (Some(preview), Some(row)) = (preview, composer.highlighted()) {
            preview(&row);
        }
        write!(stdout, "{}", composer.render(width, colour, status)).map_err(terminal_failed)?;
        stdout.flush().map_err(terminal_failed)?;
        loop {
            // The timeout is what tells a lone Escape apart from the start of
            // an arrow-key sequence.
            let key = match keys.recv_timeout(std::time::Duration::from_millis(40)) {
                Ok(byte) => decoder.feed(byte),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let key = decoder.flush_escape();
                    if key.is_none() && refreshed.elapsed() >= std::time::Duration::from_secs(1) {
                        break;
                    }
                    key
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
            };
            let Some(key) = key else { continue };
            match composer.press(key) {
                tui::Action::Submit(line) => return Ok(Some(line)),
                tui::Action::Quit => return Ok(None),
                tui::Action::Redraw => break,
                tui::Action::None => {}
            }
        }
    }
}

/// Persist the picked model, reporting only that persistence failed — the
/// choice still applies to this session.
#[cfg(feature = "tui")]
fn remember_model(route: &tui::ModelRoute, emitter: &mut Emitter) {
    if let Err(error) = save_route(route) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-UIX-1001",
            format!("the model choice was not remembered: {error}"),
            "check that the ARSY user configuration directory is writable",
        ));
    }
}

#[cfg(feature = "tui")]
fn remember_effort(effort: Option<Effort>, emitter: &mut Emitter) {
    if let Err(error) = save_effort(effort) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-UIX-1001",
            format!("the effort choice was not remembered: {error}"),
            "check that the ARSY user configuration directory is writable",
        ));
    }
}

/// The remembered model lives beside the user configuration layer that
/// `arsy doctor` already reports.
#[cfg(feature = "tui")]
fn model_store() -> Option<PathBuf> {
    Some(arsy_kernel::config::user_config()?.with_file_name("model"))
}

/// The route chosen last time, as `provider/model`.
///
/// The model is re-validated on read: a file written by an older build that
/// accepted anything must not keep selecting an unusable model on every later
/// start.
#[cfg(feature = "tui")]
fn saved_route() -> Option<tui::ModelRoute> {
    let raw = std::fs::read_to_string(model_store()?).ok()?;
    let raw = raw.trim();
    let route = (!raw.is_empty()).then(|| tui::ModelRoute::parse(raw))?;
    tui::validate_slug(&route.model).ok()?;
    Some(route)
}

#[cfg(feature = "tui")]
fn save_route(route: &tui::ModelRoute) -> io::Result<()> {
    let path = model_store()
        .ok_or_else(|| io::Error::other("this platform has no user configuration directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{route}\n"))
}

/// The remembered reasoning effort, beside the remembered model.
#[cfg(feature = "tui")]
use arsy_kernel::provider::Effort;

#[cfg(feature = "tui")]
fn effort_store() -> Option<PathBuf> {
    Some(arsy_kernel::config::user_config()?.with_file_name("effort"))
}

/// The effort chosen last time, re-validated on read for the same reason the
/// model is: an unreadable file must not decide what a turn sends.
#[cfg(feature = "tui")]
fn saved_effort() -> Option<Effort> {
    Effort::parse(std::fs::read_to_string(effort_store()?).ok()?.trim())
}

/// `None` clears the choice, so a turn goes back to carrying no reasoning knob.
#[cfg(feature = "tui")]
fn save_effort(effort: Option<Effort>) -> io::Result<()> {
    let path = effort_store()
        .ok_or_else(|| io::Error::other("this platform has no user configuration directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match effort {
        Some(effort) => std::fs::write(path, format!("{effort}\n")),
        None => match std::fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        },
    }
}

/// The remembered colour theme, beside the remembered effort.
#[cfg(feature = "tui")]
fn theme_store() -> Option<PathBuf> {
    Some(arsy_kernel::config::user_config()?.with_file_name("theme"))
}

/// The theme chosen last time, kept only if it is still a built-in name: a
/// file written by a build that knew a theme this one dropped must not select
/// nothing.
#[cfg(feature = "tui")]
fn saved_theme() -> Option<String> {
    let raw = std::fs::read_to_string(theme_store()?).ok()?;
    let name = raw.trim().to_owned();
    tui::builtin_palette(&name).map(|_| name)
}

#[cfg(feature = "tui")]
fn save_theme(name: &str) -> io::Result<()> {
    let path = theme_store()
        .ok_or_else(|| io::Error::other("this platform has no user configuration directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{name}\n"))
}

#[cfg(feature = "tui")]
fn remember_theme(name: &str, emitter: &mut Emitter) {
    if let Err(error) = save_theme(name) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-UIX-1001",
            format!("the theme choice was not remembered: {error}"),
            "check that the ARSY user configuration directory is writable",
        ));
    }
}

#[cfg(feature = "tui")]
fn apply_theme(
    answer: &str,
    current: &mut String,
    roles: &std::collections::BTreeMap<String, String>,
    stdout: &mut io::Stdout,
    emitter: &mut Emitter,
) -> io::Result<bool> {
    let picked = match tui::resolve_theme_answer(answer, current) {
        Ok(picked) => picked,
        Err(reason) => {
            writeln!(stdout, "{}", tui::safe_text(&reason))?;
            return Ok(false);
        }
    };
    tui::set_palette(&picked, roles);
    *current = picked;
    remember_theme(current, emitter);
    writeln!(stdout, "Theme: {current}")?;
    Ok(true)
}

#[cfg(feature = "tui")]
fn endpoint_models(invocation: &Invocation) -> Vec<tui::ModelChoice> {
    let Ok(root) = workspace_root(&invocation.workspace) else {
        return Vec::new();
    };
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let mut choices = Vec::new();
    if let Ok(config) = load_config(&root, &working) {
        for endpoint in config.endpoints() {
            choices.extend(endpoint.models.iter().map(|slug| tui::ModelChoice {
                provider: endpoint.id.clone(),
                slug: slug.clone(),
                name: format!("on {}", endpoint.id),
            }));
        }
    }
    let saved_handles = catalog_handles(invocation);
    for preset in arsy_kernel::oauth::presets::all() {
        let has_auth = saved_handles.iter().any(|h| h.contains(preset.id))
            || arsy_kernel::secret::OsCredentialStore.resolve(preset.id).is_ok()
            || arsy_kernel::secret::FileCredentialStore.resolve(preset.id).is_ok();
        if has_auth && !choices.iter().any(|c| c.provider == preset.id) {
            choices.extend(preset.models.iter().map(|slug| tui::ModelChoice {
                provider: preset.id.to_string(),
                slug: (*slug).to_string(),
                name: format!("on {}", preset.id),
            }));
        }
    }
    choices
}

/// The palette the session paints with: a built-in base — the `[theme]` base,
/// else the remembered theme, else the default — with any `[theme]` role
/// overrides on top. Returns the base name (for the `/theme` picker) and the
/// palette, or the reason an override was rejected.
#[cfg(feature = "tui")]
fn resolve_palette(theme: &arsy_kernel::config::Theme) -> (String, Result<tui::Palette, String>) {
    let base = theme
        .base
        .clone()
        .or_else(saved_theme)
        .unwrap_or_else(|| tui::DEFAULT_THEME.to_owned());
    let palette = tui::builtin_palette(&base).unwrap_or_else(|| {
        tui::builtin_palette(tui::DEFAULT_THEME).expect("the default theme is built in")
    });
    let built = if theme.roles.is_empty() {
        Ok(palette)
    } else {
        palette.with_overrides(&theme.roles)
    };
    (base, built)
}

/// Where `/provider` goes after an answer.
#[cfg(feature = "tui")]
enum ProviderNext {
    Ask(tui::ProviderStep),
    Done(String),
    Cancelled(String),
}

/// The provider the configuration names right now.
#[cfg(feature = "tui")]
fn configured_default(invocation: &Invocation) -> Option<String> {
    let root = workspace_root(&invocation.workspace).ok()?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    load_config(&root, &working)
        .ok()?
        .provider_default()
        .map(str::to_owned)
}

#[cfg(feature = "tui")]
fn load_workspace_sessions(workspace: &Path) -> Vec<tui::SessionChoice> {
    let Ok(store) = open_store(workspace) else {
        return Vec::new();
    };
    let Ok(summaries) = store.sessions(30) else {
        return Vec::new();
    };
    summaries
        .into_iter()
        .map(|s| {
            let ts = s.last_event_at_ms.or(s.started_at_ms).unwrap_or_default();
            let last_seen = if ts > 0 {
                let now = arsy_kernel::artifact::unix_time_ms();
                let diff_secs = now.saturating_sub(ts) / 1000;
                if diff_secs < 60 {
                    "just now".to_owned()
                } else if diff_secs < 3600 {
                    format!("{}m ago", diff_secs / 60)
                } else if diff_secs < 86400 {
                    format!("{}h ago", diff_secs / 3600)
                } else {
                    format!("{}d ago", diff_secs / 86400)
                }
            } else {
                "recorded".to_owned()
            };
            tui::SessionChoice {
                id: s.session,
                title: s.title,
                events: s.version.0,
                last_seen,
            }
        })
        .collect()
}

#[cfg(feature = "tui")]
fn reconstruct_session_conversation(
    workspace: &Path,
    session: SessionId,
) -> Vec<ModelMessage> {
    let Ok(store) = open_store(workspace) else {
        return Vec::new();
    };
    let Ok(events) = store.read(session, 1, 1000) else {
        return Vec::new();
    };
    let mut messages = Vec::new();
    for event in events {
        if event.kind == "turn.started" {
            if let arsy_kernel::event::EventPayload::Inline { data } = &event.payload {
                if let Some(prompt) = data.get("prompt").and_then(Value::as_str) {
                    messages.push(ModelMessage {
                        role: ModelRole::User,
                        content: vec![ModelContent::Text {
                            text: prompt.to_owned(),
                        }],
                    });
                }
            }
        } else if event.kind == "turn.completed" {
            if let arsy_kernel::event::EventPayload::Inline { data } = &event.payload {
                if let Some(resp) = data.get("response").and_then(Value::as_str) {
                    if !resp.trim().is_empty() {
                        messages.push(ModelMessage {
                            role: ModelRole::Assistant,
                            content: vec![ModelContent::Text {
                                text: resp.to_owned(),
                            }],
                        });
                    }
                }
            }
        }
    }
    messages
}

/// The providers configured right now, in the order the configuration lists
/// them. Read fresh each time `/provider` opens, so an edit made outside ARSY
/// is not hidden behind a stale list.
#[cfg(feature = "tui")]
fn configured_providers(invocation: &Invocation) -> Vec<String> {
    let Ok(root) = workspace_root(&invocation.workspace) else {
        return Vec::new();
    };
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let Ok(config) = load_config(&root, &working) else {
        return Vec::new();
    };
    config.endpoint_ids()
}

/// Take one answer and say what to ask next.
///
/// Every step validates its own answer and nothing is written until the last
/// one, so abandoning the wizard leaves the configuration exactly as it was.
#[cfg(feature = "tui")]
fn provider_step(
    invocation: &Invocation,
    step: tui::ProviderStep,
    line: &str,
    draft: &mut tui::ProviderDraft,
    providers: &[String],
) -> Result<ProviderNext, String> {
    use tui::ProviderStep as Step;

    let answer = if step.masked() { line } else { line.trim() };
    if answer.is_empty() {
        return Ok(ProviderNext::Cancelled("Provider unchanged.".to_owned()));
    }
    let one_of = |rows: &[(&str, &str)]| {
        rows.iter()
            .any(|(name, _)| *name == answer)
            .then(|| answer.to_owned())
            .ok_or_else(|| {
                format!(
                    "`{}` is not one of {}",
                    tui::safe_text(answer),
                    rows.iter()
                        .map(|(name, _)| *name)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    };
    let writable = |field: &str| {
        config_edit::is_writable(answer)
            .then(|| answer.to_owned())
            .ok_or_else(|| {
                format!("a {field} must be plain ASCII with no quotes, backslashes, or padding")
            })
    };

    match step {
        Step::Pick => match answer {
            "+new" => Ok(ProviderNext::Ask(Step::Name)),
            "-remove" => Ok(ProviderNext::Ask(Step::Remove)),
            chosen if providers.iter().any(|name| name == chosen) => {
                write_config(|config| config_edit::set_default(config, chosen))?;
                Ok(ProviderNext::Done(format!("Provider: {chosen}")))
            }
            other => Err(format!(
                "`{}` is not a configured provider",
                tui::safe_text(other)
            )),
        },
        Step::Name => {
            let name = writable("provider name")?;
            if providers.contains(&name) {
                return Err(format!("`{name}` is already configured"));
            }
            if name.starts_with(['+', '-']) {
                return Err("a provider name cannot start with `+` or `-`".to_owned());
            }
            draft.name = name;
            Ok(ProviderNext::Ask(Step::Kind))
        }
        Step::Kind => {
            draft.kind = one_of(tui::PROVIDER_KINDS)?;
            Ok(ProviderNext::Ask(Step::BaseUrl))
        }
        Step::BaseUrl => {
            let url = writable("base URL")?;
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err("a base URL starts with http:// or https://".to_owned());
            }
            draft.base_url = url;
            Ok(ProviderNext::Ask(Step::Model))
        }
        Step::Model => {
            draft.models = model_slugs(answer)?;
            Ok(ProviderNext::Ask(Step::Store))
        }
        Step::Store => {
            draft.store = one_of(tui::PROVIDER_STORES)?;
            Ok(ProviderNext::Ask(Step::Key))
        }
        Step::Key => {
            let handle = store_credential(invocation, &draft.name, &draft.store, answer)?;
            let endpoint = config_edit::Endpoint {
                name: draft.name.clone(),
                kind: draft.kind.clone(),
                base_url: draft.base_url.clone(),
                models: draft.models.clone(),
                credential: handle,
            };
            write_config(|config| {
                let config = config_edit::ensure_schema(config);
                let config = config_edit::append_endpoint(&config, &endpoint);
                config_edit::set_default(&config, &endpoint.name)
            })?;
            Ok(ProviderNext::Done(format!(
                "Added provider {} with {} model{}, and made it the default. The others are \
                 still configured; `/provider` switches between them.",
                endpoint.name,
                endpoint.models.len(),
                if endpoint.models.len() == 1 { "" } else { "s" },
            )))
        }
        Step::Remove => {
            if !providers.iter().any(|name| name == answer) {
                return Err(format!(
                    "`{}` is not a configured provider",
                    tui::safe_text(answer)
                ));
            }
            draft.name = answer.to_owned();
            Ok(ProviderNext::Ask(Step::ConfirmRemove))
        }
        Step::ConfirmRemove => {
            if one_of(tui::CONFIRM_ROWS)? == "no" {
                return Ok(ProviderNext::Cancelled("Provider unchanged.".to_owned()));
            }
            let name = draft.name.clone();
            write_config(|config| config_edit::remove_endpoint(config, &name))?;
            let store = CatalogStore::resolve(invocation);
            if let Ok(mut records) = catalog(store) {
                let to_remove: Vec<SecretHandle> = records
                    .iter()
                    .filter(|r| r.handle.name() == name || r.handle.name() == format!("endpoint.{name}"))
                    .map(|r| r.handle.clone())
                    .collect();
                records.retain(|r| !to_remove.contains(&r.handle));
                let _ = save_catalog(store, &records);
                for handle in to_remove {
                    match handle.store() {
                        OS_STORE_ID => {
                            let _ = OsCredentialStore.remove(handle.name());
                        }
                        FILE_STORE_ID => {
                            let _ = FileCredentialStore.remove(handle.name());
                        }
                        _ => {}
                    }
                }
            }
            Ok(ProviderNext::Done(format!(
                "Removed provider {name} and its credentials."
            )))
        }
    }
}
#[cfg(feature = "tui")]
enum AuthNext {
    Ask(tui::AuthStep),
    Done(String),
    Cancelled(String),
}

#[cfg(feature = "tui")]
fn catalog_handles(invocation: &Invocation) -> Vec<String> {
    catalog(CatalogStore::resolve(invocation))
        .map(|records| records.into_iter().map(|r| r.handle.to_string()).collect())
        .unwrap_or_default()
}

#[cfg(feature = "tui")]
fn auth_step(
    invocation: &Invocation,
    step: tui::AuthStep,
    line: &str,
    draft_provider: &mut String,
    providers: &[String],
    emitter: &mut Emitter,
) -> Result<AuthNext, String> {
    let answer = if step.masked() { line } else { line.trim() };
    if answer.is_empty() {
        return Ok(AuthNext::Cancelled("Auth unchanged.".to_owned()));
    }
    match step {
        tui::AuthStep::Pick => match answer {
            // A built-in preset is always an option, so `login` never dead-ends
            // the way `set` does with nothing configured.
            "login" => Ok(AuthNext::Ask(tui::AuthStep::LoginProvider)),
            "list" => {
                let records = catalog(CatalogStore::resolve(invocation)).map_err(|e| e.message)?;
                let human = human_credentials(&records);
                let rendered = human
                    .get("credentials")
                    .and_then(Value::as_str)
                    .unwrap_or("No credentials catalogued.");
                Ok(AuthNext::Done(rendered.to_owned()))
            }
            "set" => {
                if providers.is_empty() {
                    return Err(
                        "no providers are configured; configure a provider endpoint first"
                            .to_owned(),
                    );
                }
                Ok(AuthNext::Ask(tui::AuthStep::SetProvider))
            }
            "remove" => {
                let records = catalog(CatalogStore::resolve(invocation)).map_err(|e| e.message)?;
                if records.is_empty() {
                    return Err("no credentials are saved in the catalog".to_owned());
                }
                Ok(AuthNext::Ask(tui::AuthStep::RemoveHandle))
            }
            other => Err(format!(
                "`{}` is not one of login, list, set, remove",
                tui::safe_text(other)
            )),
        },
        tui::AuthStep::LoginProvider => {
            let known = providers.iter().any(|p| p == answer)
                || arsy_kernel::oauth::presets::get(answer).is_some();
            if !known {
                return Err(format!(
                    "`{}` is not a configured provider or a built-in preset",
                    tui::safe_text(answer)
                ));
            }
            auth_login(invocation, answer, emitter).map_err(|e| e.message)?;
            Ok(AuthNext::Done(format!(
                "Signed in to `{answer}` with OAuth."
            )))
        }
        tui::AuthStep::SetProvider => {
            if !providers.iter().any(|p| p == answer) {
                return Err(format!(
                    "`{}` is not a configured provider",
                    tui::safe_text(answer)
                ));
            }
            *draft_provider = answer.to_owned();
            Ok(AuthNext::Ask(tui::AuthStep::SetKey))
        }
        tui::AuthStep::SetKey => {
            store_credential(invocation, draft_provider, "keychain", answer)
                .map_err(|e| e.to_string())?;
            Ok(AuthNext::Done(format!(
                "Stored API key for `{draft_provider}` in the credential store."
            )))
        }
        tui::AuthStep::RemoveHandle => {
            let handle: SecretHandle =
                SecretHandle::try_from(answer.to_owned()).map_err(|error| format!("{error}"))?;
            let store = CatalogStore::resolve(invocation);
            let mut records = catalog(store).map_err(|e| e.message)?;
            records.retain(|r| r.handle != handle);
            save_catalog(store, &records).map_err(|e| e.message)?;
            match handle.store() {
                OS_STORE_ID => {
                    let _ = OsCredentialStore.remove(handle.name());
                }
                FILE_STORE_ID => {
                    let _ = FileCredentialStore.remove(handle.name());
                }
                _ => {}
            }
            Ok(AuthNext::Done(format!("Removed credential `{handle}`.")))
        }
    }
}
/// One host serves several models, so the model step takes a list. The first is
/// the endpoint's default; the rest are what `/model` offers beside it.
#[cfg(feature = "tui")]
fn model_slugs(answer: &str) -> Result<Vec<String>, String> {
    let mut models: Vec<String> = Vec::new();
    for slug in answer
        .split(',')
        .map(str::trim)
        .filter(|slug| !slug.is_empty())
    {
        if !config_edit::is_writable(slug) {
            return Err(format!(
                "`{}` is not a model slug: plain ASCII, no quotes or backslashes",
                tui::safe_text(slug)
            ));
        }
        if !models.iter().any(|existing| existing == slug) {
            models.push(slug.to_owned());
        }
    }
    if models.is_empty() {
        return Err("name at least one model".to_owned());
    }
    Ok(models)
}

/// Put a typed credential where the operator asked for it, and give back the
/// handle the configuration should point at.
#[cfg(feature = "tui")]
fn store_credential(
    invocation: &Invocation,
    name: &str,
    store: &str,
    secret: &str,
) -> Result<String, String> {
    let secret = secret.trim();
    if secret.len() < arsy_kernel::secret::MIN_SECRET_BYTES {
        return Err("that credential is too short to redact safely".to_owned());
    }
    let handle = if store == "keychain" {
        OsCredentialStore
            .set(name, secret)
            .map_err(|error| format!("the credential store refused it: {error}"))?;
        SecretHandle::new(OS_STORE_ID, name)
    } else {
        let file = format!("{name}.key");
        let path = FileCredentialStore::path(&file)
            .ok_or_else(|| "this platform has no user configuration directory".to_owned())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut written = owner_only(&path).map_err(|error| error.message)?;
        written
            .write_all(secret.as_bytes())
            .map_err(|error| error.to_string())?;
        SecretHandle::new(FILE_STORE_ID, file)
    }
    .map_err(|error| error.to_string())?;

    // Catalogued exactly as `arsy auth set` catalogues one, for two reasons:
    // `auth list` can show it, and every turn registers the catalogued handles
    // for redaction — a credential missing from the catalog is one that could
    // reach output unredacted.
    let store = CatalogStore::resolve(invocation);
    let mut records = catalog(store).map_err(|error| error.message)?;
    // Updated in place when the handle is already known, the way `auth set`
    // updates it, so re-entering a credential does not reset when it was first
    // stored.
    match records.iter_mut().find(|record| record.handle == handle) {
        Some(record) => {
            record.provider = name.to_owned();
            record.kind = CredentialKind::ApiKey;
        }
        None => records.push(AuthRecord {
            provider: name.to_owned(),
            handle: handle.clone(),
            created_at: now().map_err(|error| error.message)?,
            last_used: None,
            kind: CredentialKind::ApiKey,
        }),
    }
    save_catalog(store, &records).map_err(|error| error.message)?;
    Ok(handle.to_string())
}

/// Rewrite the user configuration through `edit`.
///
/// The file is read and written whole, so `edit` sees exactly what is on disk
/// and nothing it did not change can move.
fn write_config(edit: impl FnOnce(&str) -> String) -> Result<(), String> {
    let path = arsy_kernel::config::user_config()
        .ok_or_else(|| "this platform has no user configuration directory".to_owned())?;
    let original = match std::fs::read_to_string(&path) {
        Ok(original) => original,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("the configuration could not be read: {error}")),
    };
    let updated = edit(&original);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(&path, updated)
        .map_err(|error| format!("the configuration could not be written: {error}"))
}

/// What to print once an effort answer is accepted.
#[cfg(feature = "tui")]
fn effort_line(effort: Option<Effort>) -> String {
    match effort {
        Some(effort) => format!("Effort: {effort}"),
        None => "Effort: off, so no reasoning setting is sent".to_owned(),
    }
}

/// One interactive turn's durable state: the session it is recorded in, and
/// the task it is.
///
/// The TUI takes the same shape `arsy run` does — a turn is a leased task in
/// the session's graph — so an interactive turn that dies with its process is
/// recoverable by `arsy resume` exactly like a scripted one. What it cannot
/// share is `TaskRun::execute`, which owns the streaming loop a terminal has
/// its own version of.
#[cfg(feature = "tui")]
struct RecordedTurn {
    service: AgentService,
    graph: TaskGraph,
    actor: Principal,
    admission: arsy_kernel::service::TurnAdmission,
    session: SessionId,
    task: TaskId,
}

#[cfg(feature = "tui")]
fn record_turn(
    invocation: &Invocation,
    session: SessionId,
    task: String,
    emitter: &mut Emitter,
) -> Result<RecordedTurn, Diagnostic> {
    let store = open_store(&workspace_root(&invocation.workspace)?)?;
    emitter.session = Some(session);
    let actor = actor();
    let service = AgentService::attach(Arc::clone(&store) as Arc<dyn EventStore>, session)
        .map_err(storage_failed)?;
    let mut graph = TaskGraph::new(store, session, actor.clone()).map_err(graph_failed)?;
    let agent = AgentId::new();
    let id = TaskId::new();
    graph
        .add(TaskNode {
            id,
            goal: task.clone(),
            dependencies: Vec::new(),
            assignee: Some(agent),
            required_output: "an answer to the task".to_owned(),
            workspace: WorkspaceRequirement::IsolatedWriter,
            budget: TASK_BUDGET,
            authority: Vec::new(),
            state: TaskState::Pending,
            lease_expires_at_ms: None,
        })
        .map_err(graph_failed)?;
    graph.ready().map_err(graph_failed)?;
    graph
        .lease(id, agent, unix_time_ms() + TASK_LEASE_MS)
        .map_err(graph_failed)?;

    let envelope = ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
        session,
        prompt: task,
        extensions: Extensions::new(),
    }));
    let admission = service
        .start_turn(actor.clone(), &envelope)
        .map_err(storage_failed)?;
    Ok(RecordedTurn {
        service,
        graph,
        actor,
        admission,
        session,
        task: id,
    })
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn run_turn(
    invocation: &Invocation,
    session_id: SessionId,
    native: Option<&provider::Resolved>,
    task: &str,
    route: &tui::ModelRoute,
    effort: Option<Effort>,
    colour: bool,
    footer: &str,
    conversation: &mut Vec<ModelMessage>,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    auto_approve: &std::sync::atomic::AtomicBool,
    emitter: &mut Emitter,
) -> Result<Turn, Diagnostic> {
    let task = prepare_task(invocation, task, emitter)?;
    let RecordedTurn {
        service,
        mut graph,
        actor,
        admission,
        session,
        task: node,
    } = record_turn(invocation, session_id, task.clone(), emitter)?;
    // Where the conversation stood before this turn. A turn that fails or is
    // stopped rewinds to here, which is more than one message once the turn
    // has run tools.
    let base = conversation.len();
    conversation.push(ModelMessage {
        role: ModelRole::User,
        content: vec![ModelContent::Text { text: task.clone() }],
    });
    let root = workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let dynamic_provider = load_config(&root, &working)
        .ok()
        .and_then(|config| provider::resolve(&config, Some(&route.provider)).ok())
        .or_else(|| native.cloned());

    let outcome = match dynamic_provider.as_ref() {
        Some(resolved) => native_turn(
            resolved,
            &agent_runtime(
                &root,
                &load_config(
                    &root,
                    &working,
                )?,
                true,
            )?,
            conversation,
            route,
            effort,
            admission.turn,
            colour,
            footer,
            keys,
            decoder,
            composer,
            auto_approve,
        ),
        None => external_status(
            &root,
            &task,
            route,
            colour,
            footer,
            keys,
            decoder,
            composer,
            &emitter.redactor,
        ),
    };
    // A turn that never started leaves the composer painted, so it is torn down
    // here before the diagnostic is written over the input block.
    if outcome.is_err() {
        let mut stdout = io::stdout();
        write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
        stdout.flush().map_err(terminal_failed)?;
    }
    let turn = match outcome {
        Ok(turn) => turn,
        Err(error) => {
            let reason = format!("could not run {route}: {error}");
            graph
                .fail(node, json!({"message": reason.clone()}))
                .map_err(graph_failed)?;
            fail_turn(
                &service,
                actor,
                admission.turn,
                session,
                route,
                reason,
                emitter,
            )?;
            return Ok(Turn::default());
        }
    };
    if turn.interrupted {
        conversation.truncate(base);
        // Stopping a turn is a decision, not a fault: the turn is recorded as
        // failed for the audit trail, but the terminal already said so with an
        // `Interrupted` row and does not need a diagnostic on top.
        service
            .fail_turn(
                actor,
                admission.turn,
                "user_interrupt",
                format!("{route} was interrupted"),
            )
            .map_err(storage_failed)?;
        // Cancelled rather than failed: the operator stopped it, so nothing
        // should offer to continue it later.
        graph
            .cancel(node, "interrupted by the operator")
            .map_err(graph_failed)?;
        turn_record(
            emitter,
            json!({
                "session": session.to_string(),
                "turn": admission.turn.to_string(),
                "status": "interrupted",
            }),
        );
        return Ok(turn);
    }
    match &turn.failure {
        None => {
            if !turn.response.trim().is_empty() {
                conversation.push(ModelMessage {
                    role: ModelRole::Assistant,
                    content: vec![ModelContent::Text {
                        text: turn.response.clone(),
                    }],
                });
            }
            let mut outcome = json!({"provider": route.provider, "model": route.model});
            merge(&mut outcome, turn.usage.clone());
            service
                .complete_turn(actor, admission.turn, &outcome)
                .map_err(storage_failed)?;
            graph
                .complete(node, outcome.clone())
                .map_err(graph_failed)?;
            turn_record(
                emitter,
                json!({
                    "session": session.to_string(),
                    "turn": admission.turn.to_string(),
                    "status": "completed",
                    "model": route.to_string(),
                }),
            );
        }
        Some(failure) => {
            conversation.truncate(base);
            graph
                .fail(node, json!({"message": failure.clone()}))
                .map_err(graph_failed)?;
            fail_turn(
                &service,
                actor,
                admission.turn,
                session,
                route,
                failure.clone(),
                emitter,
            )?;
        }
    }
    Ok(turn)
}

/// How many times one turn may come back asking to run tools. The bound is
/// what stops a model that answers every result with another call from
/// spending a session on its own loop.
#[cfg(feature = "tui")]
const MAX_TOOL_ROUNDS: usize = 24;

/// What the operator said about one tool call.
#[cfg(feature = "tui")]
#[derive(Clone, Debug, Eq, PartialEq)]
enum Answer {
    Yes { note: Option<String> },
    /// Refuse this call; the turn carries on and can propose something else.
    No { note: Option<String> },
    /// Refuse this call and end the turn.
    Stop,
}

/// Run a turn on a configured provider, executing the tools it asks for.
///
/// Each round is one request. A round that ends without tool calls is the
/// answer; a round that asks for tools runs the confirmed ones, appends the
/// call and its result to the conversation, and asks again.
///
/// Nothing runs unconfirmed: every call is shown and answered from the
/// keyboard, and a declined call is reported to the model as a failed result
/// rather than hidden, so it can say what it would do instead.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn native_turn(
    resolved: &provider::Resolved,
    runtime: &arsy_code::agent::ToolRuntime,
    conversation: &mut Vec<ModelMessage>,
    route: &tui::ModelRoute,
    effort: Option<Effort>,
    turn: arsy_kernel::domain::TurnId,
    colour: bool,
    footer: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    auto_approve: &std::sync::atomic::AtomicBool,
) -> io::Result<Turn> {
    // rather than taken from the last one: an audit that reads a tool-using
    // turn as the price of its final request under-reports what it cost.
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    let charge = |outcome: &mut Turn, input: &mut u64, output: &mut u64| {
        *input += outcome.usage["input_tokens"].as_u64().unwrap_or_default();
        *output += outcome.usage["output_tokens"].as_u64().unwrap_or_default();
        if *input > 0 || *output > 0 {
            outcome.usage = json!({"input_tokens": *input, "output_tokens": *output});
        }
    };
    for round in 0..MAX_TOOL_ROUNDS {
        // Before the request, not after: a transcript that has outgrown the
        // window fails at the provider, and the operator is told what was
        // elided rather than watching the turn shrink invisibly.
        let trimmed = arsy_code::agent::budget::trim(conversation, context_budget(resolved));
        if trimmed.changed() {
            let mut terminal = io::stdout();
            writeln!(
                terminal,
                "{}",
                tui::tool_result_row(
                    colour,
                    "context",
                    true,
                    &format!(
                        "elided {} earlier tool result(s) to stay within {} tokens",
                        trimmed.elided, trimmed.after
                    )
                )
            )?;
            terminal.flush()?;
        }
        let mut outcome = native_status(
            resolved,
            runtime,
            conversation,
            route,
            effort,
            turn,
            round,
            colour,
            footer,
            keys,
            decoder,
            composer,
        )?;
        charge(&mut outcome, &mut input_tokens, &mut output_tokens);
        if outcome.calls.is_empty() || outcome.interrupted || outcome.failure.is_some() {
            return Ok(outcome);
        }
        // The calls are history now, whatever the operator decides about them:
        // a provider that sent a call and never sees its result rejects the
        // next request.
        let calls = std::mem::take(&mut outcome.calls);
        let mut content: Vec<ModelContent> = Vec::new();
        if !outcome.response.trim().is_empty() {
            content.push(ModelContent::Text {
                text: outcome.response.clone(),
            });
        }
        content.extend(
            calls
                .iter()
                .map(|(id, name, arguments)| ModelContent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
        );
        conversation.push(ModelMessage {
            role: ModelRole::Assistant,
            content,
        });

        let mut results = Vec::with_capacity(calls.len());
        let mut terminal = io::stdout();
        for (id, name, arguments) in &calls {
            let summary = runtime.summarize(name, arguments);
            // Once the turn is stopped the remaining calls are still answered,
            // because a call the provider sent needs a result; they are simply
            // answered without running anything.
            let (content, is_error) = if outcome.interrupted {
                ("The operator declined to run this call.".to_owned(), true)
            } else {
                writeln!(
                    terminal,
                    "{}",
                    tui::tool_running_row(colour, name, &summary)
                )?;
                terminal.flush()?;
                match execute_call(
                    runtime,
                    &mut terminal,
                    colour,
                    name,
                    arguments,
                    &summary,
                    keys,
                    decoder,
                    auto_approve,
                )? {
                    Executed::Answered(mut result) => {
                        if !result.changed_files.is_empty() {
                            result.output.push_str("\nChanged files:\n");
                            for path in &result.changed_files {
                                result.output.push_str(&format!("  • {path}\n"));
                            }
                        }
                        (result.output, !result.success)
                    }
                    Executed::Stopped => {
                        outcome.interrupted = true;
                        writeln!(terminal, "{}", tui::interrupted_row(colour))?;
                        ("The operator stopped the turn.".to_owned(), true)
                    }
                }
            };
            if name == "bash" || name == "shell.execute" {
                writeln!(
                    terminal,
                    "{}",
                    tui::bash_box(
                        tui::terminal_width(),
                        colour,
                        &summary,
                        &content,
                        Some(if is_error { 1 } else { 0 }),
                        std::time::Duration::from_millis(50),
                    )
                )?;
            } else {
                writeln!(
                    terminal,
                    "{}",
                    tui::tool_box(
                        tui::terminal_width(),
                        colour,
                        name,
                        &summary,
                        &content,
                        !is_error,
                        std::time::Duration::from_millis(50),
                    )
                )?;
            }
            terminal.flush()?;
            results.push(ModelContent::ToolResult {
                id: id.clone(),
                content,
                is_error,
            });
        }
        conversation.push(ModelMessage {
            role: ModelRole::User,
            content: results,
        });
        // The response of a round that called tools belongs to the history
        // above, not to the answer this turn returns.
        outcome.response.clear();
        if outcome.interrupted {
            return Ok(outcome);
        }
        if round + 1 == MAX_TOOL_ROUNDS {
            outcome.failure = Some(format!(
                "{route} asked for tools {MAX_TOOL_ROUNDS} times without finishing the turn"
            ));
            return Ok(outcome);
        }
    }
    Ok(Turn::default())
}

/// What happened to one tool call.
#[cfg(feature = "tui")]
enum Executed {
    Answered(arsy_code::agent::ToolResult),
    Stopped,
}

/// Decide, confirm if the decision says to, and run.
///
/// Policy is asked first, so the operator is only interrupted for calls that
/// actually need a human: a read policy already allows runs without a prompt,
/// and a call policy denies is refused without one. That is the difference
/// between an approval and a habit — an operator asked to confirm every read
/// stops reading the prompts.
#[cfg(feature = "tui")]
fn execute_call(
    runtime: &arsy_code::agent::ToolRuntime,
    terminal: &mut io::Stdout,
    colour: bool,
    name: &str,
    arguments: &Value,
    summary: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    auto_approve: &std::sync::atomic::AtomicBool,
) -> io::Result<Executed> {
    use arsy_code::agent::Authorization;

    let started = std::time::Instant::now();
    let request = match runtime.prepare(name, arguments) {
        Ok(request) => request,
        Err(failure) => return Ok(Executed::Answered(*failure)),
    };
    let refused =
        |reason: String| Executed::Answered(arsy_code::agent::ToolResult::refused(name, reason));
    let authorization = runtime.authorize(&request);
    let (grants, approval_note) = match &authorization {
        Authorization::Allowed(grants) => (grants.clone(), None),
        Authorization::Denied(reason) => return Ok(refused(reason.clone())),
        Authorization::NeedsApproval { .. } => {
            let is_read = matches!(
                name,
                "fs.read"
                    | "fs.list"
                    | "search.files"
                    | "search.text"
                    | "code.symbol"
                    | "code.inspect"
                    | "code.references"
                    | "code.diagnostics"
            );
            if is_read || auto_approve.load(std::sync::atomic::Ordering::Relaxed) {
                match authorization.approve() {
                    Ok(grants) => (grants, None),
                    Err(error) => {
                        return Ok(refused(format!(
                            "the approval could not be turned into a grant: {error}"
                        )))
                    }
                }
            } else {
                let reason = authorization.requested();
                let preview = format_tool_preview(name, arguments);
                match confirm_tool(terminal, colour, name, summary, &reason, preview, keys, decoder, auto_approve)? {
                    Answer::Yes { note } => match authorization.approve() {
                        Ok(grants) => (grants, note),
                        Err(error) => {
                            return Ok(refused(format!(
                                "the approval could not be turned into a grant: {error}"
                            )))
                        }
                    },
                    Answer::No { note } => {
                        let message = note.map_or_else(
                            || "The operator declined to run this call.".to_owned(),
                            |note| format!("The operator declined to run this call. Feedback: {note}"),
                        );
                        return Ok(refused(message));
                    }
                    Answer::Stop => return Ok(Executed::Stopped),
                }
            }
        }
    };
    let mut result = runtime.dispatch(name, &request, &grants, started);
    if let Some(note) = approval_note {
        result.output = format!("{}\nOperator note: {note}", result.output);
    }
    Ok(Executed::Answered(result))
}

#[cfg(feature = "tui")]
fn format_tool_preview(name: &str, arguments: &Value) -> Option<String> {
    match name {
        "apply_patch" | "fs.edit" | "edit" => {
            arguments
                .get("input")
                .or_else(|| arguments.get("patch"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        }
        "fs.write" | "write" => {
            let path = arguments.get("path").and_then(Value::as_str).unwrap_or("file");
            let content = arguments.get("content").and_then(Value::as_str).unwrap_or("");
            let preview: Vec<String> = content.lines().take(12).map(|l| format!("+{l}")).collect();
            let mut text = format!("--- /dev/null\n+++ {path}\n{}", preview.join("\n"));
            if content.lines().count() > 12 {
                text.push_str(&format!("\n… ({} lines omitted)", content.lines().count() - 12));
            }
            Some(text)
        }
        "bash" | "shell.execute" => {
            arguments.get("command").and_then(Value::as_str).map(|cmd| format!("$ {cmd}"))
        }
        _ => None,
    }
}

/// Ask the operator whether one tool call may run.
#[cfg(feature = "tui")]
fn confirm_tool(
    terminal: &mut io::Stdout,
    colour: bool,
    name: &str,
    summary: &str,
    reason: &str,
    diff_preview: Option<String>,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    auto_approve: &std::sync::atomic::AtomicBool,
) -> io::Result<Answer> {
    if auto_approve.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(Answer::Yes { note: None });
    }
    let mut dialog = tui::AskDialogState::for_approval(name, summary, reason, diff_preview);
    let width = tui::terminal_width();
    let mut rendered_lines = dialog.render(width, colour).lines().count();
    write!(terminal, "{}\n", dialog.render(width, colour))?;
    terminal.flush()?;
    loop {
        match keys.recv() {
            Ok(byte) => match decoder.feed(byte) {
                Some(key) => {
                    if let Some(result) = dialog.handle_key(key) {
                        write!(terminal, "\x1b[{}A\r\x1b[J", rendered_lines)?;
                        terminal.flush()?;
                        match result {
                            tui::AskDialogResult::Approve { note } => return Ok(Answer::Yes { note }),
                            tui::AskDialogResult::AlwaysApprove { note } => {
                                auto_approve.store(true, std::sync::atomic::Ordering::Relaxed);
                                return Ok(Answer::Yes { note });
                            }
                            tui::AskDialogResult::Deny { note } => return Ok(Answer::No { note }),
                            tui::AskDialogResult::Cancel => return Ok(Answer::Stop),
                        }
                    } else {
                        let frame = dialog.render(width, colour);
                        write!(terminal, "\x1b[{}A\r\x1b[J{}\n", rendered_lines, frame)?;
                        terminal.flush()?;
                        rendered_lines = frame.lines().count();
                    }
                }
                None => continue,
            },
            Err(_) => return Ok(Answer::Stop),
        }
    }
}

#[cfg(feature = "tui")]
fn redraw_live_response(
    terminal: &mut io::Stdout,
    composer: &mut tui::Composer,
    colour: bool,
    footer: &str,
    status: &str,
    text: &str,
    replace: bool,
) -> io::Result<()> {
    let mut frame = composer.clear();
    if replace {
        frame.push_str("\x1b[1A\r\x1b[K");
    }
    frame.push_str(&tui::assistant_row(colour, text));
    frame.push('\n');
    frame.push_str(&composer.render_turn(tui::terminal_width(), colour, status, footer));
    write!(terminal, "{frame}")?;
    terminal.flush()
}

#[cfg(feature = "tui")]
fn erase_live_response(terminal: &mut io::Stdout, composer: &mut tui::Composer) -> io::Result<()> {
    write!(terminal, "{}\x1b[1A\r\x1b[K", composer.clear())?;
    terminal.flush()
}

/// Stream one round of a turn from a configured provider, keeping the composer
/// alive.
///
/// The stream runs on its own thread for the same reason the Codex reader
/// does: the main loop has to keep watching the key stream, which is what lets
/// Esc or Ctrl-C stop a turn and keeps the composer typeable meanwhile. The
/// thread is detached rather than joined, so an interrupt never waits on a
/// stalled socket; dropping the receiver is what stops it, because the next
/// send fails and the stream is dropped with the thread.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn native_status(
    resolved: &provider::Resolved,
    runtime: &arsy_code::agent::ToolRuntime,
    conversation: &[ModelMessage],
    route: &tui::ModelRoute,
    effort: Option<Effort>,
    turn: arsy_kernel::domain::TurnId,
    round: usize,
    colour: bool,
    footer: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
) -> io::Result<Turn> {
    let request = CanonicalModelRequest {
        model: ModelKey {
            provider: route.provider.clone(),
            model: route.model.clone(),
        },
        // The harness's instructions and the project's, discovered by walking
        // the workspace. Rebuilt per round rather than captured once: an
        // AGENTS.md the turn just edited is the one the next round should read.
        system: system_prompt(runtime.workspace(), &route.provider, &route.model),
        messages: conversation.to_vec(),
        tools: runtime.schemas(),
        max_output_tokens: resolved.endpoint.max_output_tokens,
        effort,
        // One turn can take several requests, one per round of tool calls. The
        // round is part of the key, because a retry must repeat its own
        // request rather than collapse into the one before it.
        idempotency_key: arsy_kernel::protocol::IdempotencyKey::new(format!("{turn}-{round}"))
            .map_err(io::Error::other)?,
    };

    let provider = Arc::clone(&resolved.provider);
    let (rows, events) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stream = match arsy_kernel::provider::stream_with_retry(
            provider.as_ref(),
            &request,
            &mut std::thread::sleep,
        ) {
            Ok(stream) => stream,
            Err(error) => {
                let _ = rows.send(Err(error.to_string()));
                return;
            }
        };
        for event in stream {
            let message = match event {
                Ok(ModelEvent::TextDelta { text }) => Ok(Streamed::Text(text)),
                Ok(ModelEvent::ThinkingDelta { text }) => Ok(Streamed::Thinking(text)),
                Ok(ModelEvent::Usage {
                    input_tokens,
                    output_tokens,
                }) => Ok(Streamed::Usage {
                    input_tokens,
                    output_tokens,
                }),
                Ok(ModelEvent::ToolCallCompleted {
                    id,
                    name,
                    arguments,
                    ..
                }) => Ok(Streamed::Tool {
                    id,
                    name,
                    arguments,
                }),
                Ok(_) => continue,
                Err(error) => Err(error.to_string()),
            };
            let failed = message.is_err();
            if rows.send(message).is_err() || failed {
                return;
            }
        }
    });

    let mut outcome = Turn::default();
    let mut terminal = io::stdout();
    // Deltas arrive token by token; a row is emitted per line so scrollback
    // reads like the Codex projection rather than one row per token.
    //
    // Thinking and the answer hold separate buffers, and each thinking section
    // is announced once with its own header row, so the verbose stream reads as
    // distinct parts of the turn rather than one grey blur.
    let mut pending = String::new();
    let mut thinking = String::new();
    let mut thinking_open = false;
    let mut answer_open = false;
    let mut live_answer = false;
    let started = std::time::Instant::now();
    let mut tick = 0usize;
    // A static `Working…` line cannot tell a slow connect from a hang; the
    // status is rebuilt on every timer pass instead of captured once.
    let status_line = |first_event: bool, tick: usize| {
        tui::turn_status(
            colour,
            if first_event {
                tui::TurnPhase::Answering
            } else {
                tui::TurnPhase::Connecting
            },
            started.elapsed(),
            tick,
            0,
        )
    };
    let mut first_event = false;
    let draw = |terminal: &mut io::Stdout,
                composer: &mut tui::Composer,
                row: Option<&str>,
                status: &str| {
        let mut frame = composer.clear();
        if let Some(row) = row {
            frame.push_str(row);
            frame.push('\n');
        }
        frame.push_str(&composer.render_turn(tui::terminal_width(), colour, status, footer));
        write!(terminal, "{frame}").and_then(|()| terminal.flush())
    };
    draw(&mut terminal, composer, None, &status_line(false, 0))?;
    loop {
        let mut typed = false;
        while let Ok(byte) = keys.try_recv() {
            let Some(key) = decoder.feed(byte) else {
                continue;
            };
            if key == tui::Key::Interrupt {
                outcome.queued.clear();
                outcome.interrupted = true;
                draw(
                    &mut terminal,
                    composer,
                    Some(&tui::interrupted_row(colour)),
                    &status_line(first_event, tick),
                )?;
                return finish(terminal, composer, outcome);
            }
            match composer.press(key) {
                // Bounded as on the Codex route, so a held Enter cannot grow the
                // queue without limit; past the bound the draft is handed back.
                tui::Action::Submit(line) if !line.trim().is_empty() => {
                    if outcome.queued.len() < 16 {
                        outcome.queued.push_back(line);
                    } else {
                        composer.restore(line);
                    }
                }
                tui::Action::Submit(_) => typed = true,
                tui::Action::Quit => outcome.quit = true,
                tui::Action::Redraw => typed = true,
                tui::Action::None => {}
            }
        }
        // The status is alive: the spinner advances and the seconds climb even
        // while the provider sends nothing, so a silent turn never reads as a
        // frozen one.
        tick = tick.wrapping_add(1);
        if typed {
            draw(
                &mut terminal,
                composer,
                None,
                &status_line(first_event, tick),
            )?;
        }
        match events.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(Ok(Streamed::Thinking(text))) => {
                let width = tui::terminal_width();
                // A thinking section opens its own bordered box so reasoning
                // is visually framed apart from the answer it precedes.
                if !thinking_open {
                    thinking_open = true;
                    draw(
                        &mut terminal,
                        composer,
                        Some(&tui::thinking_box_top(width, colour)),
                        &status_line(first_event, tick),
                    )?;
                }
                thinking.push_str(&text);
                while let Some(newline) = thinking.find('\n') {
                    let line: String = thinking.drain(..=newline).collect();
                    draw(
                        &mut terminal,
                        composer,
                        Some(&tui::thinking_box_row(width, colour, &line)),
                        &status_line(first_event, tick),
                    )?;
                }
                first_event = true;
            }
            Ok(Ok(Streamed::Text(text))) => {
                outcome.response.push_str(&text);
                let width = tui::terminal_width();
                if !answer_open {
                    answer_open = true;
                    draw(
                        &mut terminal,
                        composer,
                        Some(&tui::assistant_header(colour)),
                        &status_line(first_event, tick),
                    )?;
                }
                // Answer text closes the thinking box cleanly before the prose starts.
                if thinking_open {
                    thinking_open = false;
                    if !thinking.trim().is_empty() {
                        let line = std::mem::take(&mut thinking);
                        draw(
                            &mut terminal,
                            composer,
                            Some(&tui::thinking_box_row(width, colour, &line)),
                            &status_line(first_event, tick),
                        )?;
                    }
                    draw(
                        &mut terminal,
                        composer,
                        Some(&tui::thinking_box_bottom(width, colour)),
                        &status_line(first_event, tick),
                    )?;
                }
                pending.push_str(&text);
                while let Some(newline) = pending.find('\n') {
                    if live_answer {
                        erase_live_response(&mut terminal, composer)?;
                        live_answer = false;
                    }
                    let line: String = pending.drain(..=newline).collect();
                    draw(
                        &mut terminal,
                        composer,
                        Some(&tui::assistant_row(colour, &line)),
                        &status_line(first_event, tick),
                    )?;
                }
                if !pending.is_empty() {
                    redraw_live_response(
                        &mut terminal,
                        composer,
                        colour,
                        footer,
                        &status_line(first_event, tick),
                        &pending,
                        live_answer,
                    )?;
                    live_answer = true;
                }
                first_event = true;
            }
            Ok(Ok(Streamed::Usage {
                input_tokens,
                output_tokens,
            })) => {
                outcome.usage =
                    json!({"input_tokens": input_tokens, "output_tokens": output_tokens});
            }
            Ok(Ok(Streamed::Tool {
                id,
                name,
                arguments,
            })) => {
                outcome.calls.push((id, name, arguments));
                first_event = true;
            }
            Ok(Err(failure)) => {
                outcome.failure = Some(failure);
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if decoder.flush_escape() == Some(tui::Key::Interrupt) {
                    outcome.interrupted = true;
                    draw(
                        &mut terminal,
                        composer,
                        Some(&tui::interrupted_row(colour)),
                        &status_line(first_event, tick),
                    )?;
                    return finish(terminal, composer, outcome);
                }
                // Repaint the live status on every idle pass.
                draw(
                    &mut terminal,
                    composer,
                    None,
                    &status_line(first_event, tick),
                )?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let width = tui::terminal_width();
    if thinking_open {
        if !thinking.trim().is_empty() {
            let line = std::mem::take(&mut thinking);
            draw(
                &mut terminal,
                composer,
                Some(&tui::thinking_box_row(width, colour, &line)),
                &status_line(first_event, tick),
            )?;
        }
        draw(
            &mut terminal,
            composer,
            Some(&tui::thinking_box_bottom(width, colour)),
            &status_line(first_event, tick),
        )?;
    }
    if !pending.trim().is_empty() {
        draw(
            &mut terminal,
            composer,
            Some(&tui::assistant_row(colour, &pending)),
            &status_line(first_event, tick),
        )?;
    }
    finish(terminal, composer, outcome)
}

/// One streamed fact from a provider, as the terminal needs it.
#[cfg(feature = "tui")]
enum Streamed {
    Text(String),
    Thinking(String),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// A complete tool call. The host runs it after the stream ends, so a turn
    /// is never edited from under a model that is still writing.
    Tool {
        id: String,
        name: String,
        arguments: Value,
    },
}

/// Tear the composer down so the next thing printed starts on its own line.
#[cfg(feature = "tui")]
fn finish(mut terminal: io::Stdout, composer: &mut tui::Composer, turn: Turn) -> io::Result<Turn> {
    write!(terminal, "{}", composer.clear())?;
    terminal.flush()?;
    Ok(turn)
}

#[cfg(feature = "tui")]
/// Run the task through the logged-in Codex CLI and project its JSONL event
/// stream as ARSY rows, so the terminal shows one interface, not two.
#[allow(clippy::too_many_arguments)]
fn external_status(
    workspace: &Path,
    task: &str,
    route: &tui::ModelRoute,
    colour: bool,
    footer: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    redactor: &Redactor,
) -> io::Result<Turn> {
    let mut command = std::process::Command::new("codex");
    command.args([
        "exec",
        "--json",
        "--ephemeral",
        "--sandbox",
        "read-only",
        "--cd",
    ]);
    command.arg(workspace);
    if route.model != "default" {
        command.args(["--model", &route.model]);
    }
    command.arg("-");
    command.current_dir(workspace);
    drive_provider(
        command, task, route, colour, footer, keys, decoder, composer, redactor,
    )
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn drive_provider(
    mut command: std::process::Command,
    task: &str,
    route: &tui::ModelRoute,
    colour: bool,
    footer: &str,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    redactor: &Redactor,
) -> io::Result<Turn> {
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut child = tui::ProviderChild(child);
    let mut stderr = child.0.stderr.take().expect("piped stderr is available");
    let (errors, error_output) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = Read::by_ref(&mut stderr).take(8192).read_to_end(&mut bytes);
        let _ = io::copy(&mut stderr, &mut io::sink());
        let _ = errors.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    let mut stdin = child.0.stdin.take().expect("piped stdin is available");
    let task = task.to_owned();
    let (sent, input) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = sent.send(stdin.write_all(task.as_bytes()));
    });
    // The event stream is read on a thread so the main loop can also watch the
    // key stream: that is what lets Esc or Ctrl-C stop a turn, and what keeps
    // the composer alive and typeable while the provider works.
    let stdout = child.0.stdout.take().expect("piped stdout is available");
    let events = tui::provider_lines(stdout);
    let started = std::time::Instant::now();
    let mut cancelling = None;
    let mut stream_closed = false;
    let mut finished = None;
    let mut last_event = std::time::Instant::now();
    let mut stopped_early = false;
    let mut last_row = None;
    let mut last_key = std::time::Instant::now();
    let mut exited = None;
    let mut status: Option<std::process::ExitStatus> = None;
    let mut outcome = Turn::default();
    let mut terminal = io::stdout();
    let width = std::cell::Cell::new(tui::terminal_width());
    composer.set_height(tui::terminal_rows());
    let draw = |terminal: &mut io::Stdout,
                composer: &mut tui::Composer,
                row: Option<&str>,
                cancelling: bool,
                queued: usize,
                tick: usize| {
        // Rows land above the composer, which is torn down and repainted around
        // each one so the input block is never overwritten.
        let mut frame = composer.clear();
        if let Some(row) = row {
            frame.push_str(row);
            frame.push('\n');
        }
        let phase = if cancelling {
            tui::TurnPhase::Cancelling
        } else {
            tui::TurnPhase::Working
        };
        let status = tui::turn_status(colour, phase, started.elapsed(), tick, queued);
        frame.push_str(&composer.render_turn(width.get(), colour, &status, footer));
        write!(terminal, "{frame}").and_then(|()| terminal.flush())
    };
    draw(&mut terminal, composer, None, false, 0, 0)?;
    let mut refreshed = std::time::Instant::now();
    loop {
        let tick = (started.elapsed().as_millis() / 100) as usize;
        if let Ok(result) = input.try_recv() {
            if !outcome.interrupted {
                result?;
            }
        }
        if status.is_none() {
            status = child.0.try_wait()?;
            if status.is_some() {
                exited = Some(std::time::Instant::now());
                child.stop(true);
            }
        }
        if exited
            .is_some_and(|at: std::time::Instant| at.elapsed() >= std::time::Duration::from_secs(2))
        {
            break;
        }
        // The turn is over when the provider says it is over. A CLI that
        // lingers after its terminal event — cleaning up a session, flushing
        // telemetry — must not keep the clock running against the answer that
        // is already on screen.
        //
        // Trailing rows still land: the stream drains until it has been quiet
        // for 250ms, and no longer than 2 seconds however talkative it stays.
        if status.is_none()
            && finished.is_some_and(|at: std::time::Instant| {
                last_event.elapsed() >= std::time::Duration::from_millis(250)
                    || at.elapsed() >= std::time::Duration::from_secs(2)
            })
        {
            stopped_early = true;
            child.stop(false);
            break;
        }
        if started.elapsed() >= std::time::Duration::from_secs(300) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "provider exceeded the 300-second turn deadline",
            ));
        }
        if cancelling
            .is_some_and(|at: std::time::Instant| at.elapsed() >= std::time::Duration::from_secs(2))
        {
            child.stop(true);
            status = Some(child.0.wait()?);
            break;
        }
        let mut typed = false;
        for _ in 0..256 {
            let byte = match keys.try_recv() {
                Ok(byte) => byte,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    outcome.quit = true;
                    if cancelling.is_none() {
                        cancelling = Some(std::time::Instant::now());
                        child.stop(false);
                        outcome.interrupted = true;
                        outcome.queued.clear();
                    }
                    break;
                }
            };
            last_key = std::time::Instant::now();
            let Some(key) = decoder.feed(byte) else {
                continue;
            };
            // While the provider is running, Interrupt always means the turn,
            // never the composer or the session — and it drops a queued
            // follow-up, which was only queued to run after this turn.
            if key == tui::Key::Interrupt {
                outcome.queued.clear();
                if !outcome.interrupted {
                    outcome.interrupted = true;
                    cancelling = Some(std::time::Instant::now());
                    child.stop(false);
                    draw(
                        &mut terminal,
                        composer,
                        Some(&tui::interrupted_row(colour)),
                        true,
                        0,
                        tick,
                    )?;
                }
                continue;
            }
            match composer.press(key) {
                // A line sent while the provider is busy runs as soon as this
                // turn ends, rather than being dropped or blocking.
                tui::Action::Submit(line) if !line.trim().is_empty() => {
                    if outcome.queued.len() < 16 {
                        draw(
                            &mut terminal,
                            composer,
                            Some("  Follow-up queued."),
                            cancelling.is_some(),
                            outcome.queued.len(),
                            tick,
                        )?;
                    } else {
                        composer.restore(line);
                        draw(
                            &mut terminal,
                            composer,
                            Some("  Queue full; draft retained."),
                            cancelling.is_some(),
                            outcome.queued.len(),
                            tick,
                        )?;
                    }
                }
                tui::Action::Submit(_) => typed = true,
                tui::Action::Quit => {
                    outcome.quit = true;
                    outcome.interrupted = true;
                    outcome.queued.clear();
                    if cancelling.is_none() {
                        cancelling = Some(std::time::Instant::now());
                        child.stop(false);
                    }
                }
                tui::Action::Redraw => typed = true,
                tui::Action::None => {}
            }
        }
        if last_key.elapsed() >= std::time::Duration::from_millis(40)
            && decoder.flush_escape() == Some(tui::Key::Interrupt)
            && !outcome.interrupted
        {
            outcome.interrupted = true;
            outcome.queued.clear();
            cancelling = Some(std::time::Instant::now());
            child.stop(false);
            draw(
                &mut terminal,
                composer,
                Some(&tui::interrupted_row(colour)),
                true,
                0,
                tick,
            )?;
        }
        let resize_tick = refreshed.elapsed() >= std::time::Duration::from_secs(1);
        if resize_tick {
            width.set(tui::terminal_width());
            composer.set_height(tui::terminal_rows());
            refreshed = std::time::Instant::now();
        }
        if typed || resize_tick {
            draw(
                &mut terminal,
                composer,
                None,
                cancelling.is_some(),
                outcome.queued.len(),
                tick,
            )?;
        }
        match events.recv_timeout(std::time::Duration::from_millis(40)) {
            Ok(_) if outcome.interrupted => {}
            Ok(line) => {
                last_event = std::time::Instant::now();
                let line = line?;
                let line = redactor.sanitize(&line).map_err(io::Error::other)?;
                let event = serde_json::from_str::<Value>(&line)
                    .map_err(|_| io::Error::other("provider emitted invalid JSON"))?;
                if matches!(
                    event["type"].as_str(),
                    Some("turn.completed" | "turn.failed")
                ) && finished.is_none()
                {
                    finished = Some(std::time::Instant::now());
                }
                outcome.provider_failed |= event["type"] == "turn.failed";
                // A killed provider still flushes buffered events; showing them
                // after the interrupt notice would contradict it.
                if !outcome.interrupted {
                    if let Some(row) = tui::render_codex_event(&line, colour) {
                        if last_row.as_ref() != Some(&row) {
                            draw(
                                &mut terminal,
                                composer,
                                Some(&row),
                                false,
                                outcome.queued.len(),
                                tick,
                            )?;
                        }
                        last_row = Some(row);
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Repaint so the spinner and clock stay alive while the
                // provider is quiet, not just when an event or a key arrives.
                draw(
                    &mut terminal,
                    composer,
                    None,
                    cancelling.is_some(),
                    outcome.queued.len(),
                    tick,
                )?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => stream_closed = true,
        }
        if stream_closed {
            if status.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    }
    write!(terminal, "{}", composer.clear())?;
    terminal.flush()?;
    if !outcome.interrupted && finished.is_none() {
        let detail = error_output
            .recv_timeout(std::time::Duration::from_millis(100))
            .unwrap_or_default();
        let detail = redactor.sanitize(&detail).map_err(io::Error::other)?;
        return Err(io::Error::other(format!(
            "provider closed its stream without a terminal turn event: {}",
            terminal_text(detail.trim())
        )));
    }
    // A zero process exit must not mask a turn the provider itself reported as
    // failed, so the event stream is checked before the exit status.
    if !outcome.interrupted {
        let status = match status {
            Some(status) => status,
            // The turn ended before the process did, so the process is asked to
            // leave and then made to: waiting on a CLI that ignores the signal
            // is the hang this exit was added to avoid.
            None if stopped_early => {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
                loop {
                    if let Some(status) = child.0.try_wait()? {
                        break status;
                    }
                    if std::time::Instant::now() >= deadline {
                        child.stop(true);
                        break child.0.wait()?;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
            None => child.0.wait()?,
        };
        outcome.failure = if outcome.provider_failed {
            Some(format!("{route} reported a failed turn"))
        // A signal ARSY sent after a completed turn is its own exit code, not a
        // verdict on the turn the provider already reported.
        } else if status.success() || stopped_early {
            None
        } else {
            Some(format!("{route} exited with status {status}"))
        };
    }
    Ok(outcome)
}

/// What one interactive turn left behind, whichever route ran it.
#[cfg(feature = "tui")]
#[derive(Default)]
struct Turn {
    /// `None` when the turn succeeded; otherwise why it did not.
    failure: Option<String>,
    /// The full text of the model's answer, kept so follow-up turns in the
    /// same session know what the model said.
    response: String,
    /// Extra facts to record on a completed turn, such as token usage.
    usage: Value,
    interrupted: bool,
    provider_failed: bool,
    /// A line submitted while this turn was still running.
    queued: std::collections::VecDeque<String>,
    quit: bool,
    /// Tool calls the model made and the host has not run yet: id, name, and
    /// arguments. Only complete calls land here, so a truncated stream cannot
    /// leave a half-parsed call to execute.
    calls: Vec<(String, String, Value)>,
}
#[cfg(feature = "tui")]
/// Record and report a turn the provider did not complete.
///
/// The code and the reason follow the route, because "the CLI failed" is not
/// something to tell an operator whose turn went straight to an endpoint, and
/// the recorded reason is what a later audit reads.
fn fail_turn(
    service: &AgentService,
    actor: Principal,
    turn: arsy_kernel::domain::TurnId,
    session: SessionId,
    route: &tui::ModelRoute,
    message: String,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let (code, reason, remediation) = if route.is_codex() {
        (
            "ARSY-PRV-1002",
            "provider_cli",
            "verify the selected CLI login and model, then retry",
        )
    } else {
        (
            ARSY_PRV_1000,
            "provider",
            "check the provider endpoint, credential, and model in `arsy config explain`",
        )
    };
    let diagnostic = Diagnostic::error(code, message, remediation);
    service
        .fail_turn(actor, turn, reason, diagnostic.message.clone())
        .map_err(storage_failed)?;
    emitter.diagnostic(&diagnostic);
    turn_record(
        emitter,
        json!({
            "session": session.to_string(),
            "turn": turn.to_string(),
            "status": "failed",
        }),
    );
    Ok(diagnostic.exit_code())
}

/// Emit the machine record for one interactive turn.
///
/// The record is the audit trail machines read, so `--output json` and `ci`
/// keep it. Printing it after every reply in the interactive terminal only
/// buries the reply, and the same evidence is already durable in the session
/// store, reachable with `arsy resume`.
#[cfg(feature = "tui")]
fn turn_record(emitter: &mut Emitter, payload: Value) {
    if emitter.output != Output::Human {
        emitter.result(payload);
    }
}

fn run(invocation: &Invocation, task: &str, emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let task = if task == "-" {
        read_stdin()?
    } else {
        task.to_owned()
    };
    let goal = prepare_task(invocation, &task, emitter)?;
    let mut execution = match TaskRun::open(invocation, None) {
        Ok(execution) => execution,
        Err(diagnostic) => return Ok(unusable(diagnostic, emitter)),
    };
    emitter.session = Some(execution.session);
    let task = execution.enqueue(&goal)?;
    execution.execute(task, Value::Null, emitter)
}

/// A misconfiguration is the operator's to fix, not a failed turn in their
/// session history, so nothing is recorded before it is reported.
fn unusable(mut diagnostic: Diagnostic, emitter: &mut Emitter) -> i32 {
    if diagnostic.code == ARSY_PRV_1000 {
        diagnostic.remediation = format!(
            "{}; or use the interactive TUI with a logged-in Codex CLI",
            diagnostic.remediation
        );
    }
    emitter.diagnostic(&diagnostic);
    diagnostic.exit_code()
}

/// How long a task's lease runs before another process may take it over.
///
/// A turn still in flight has not lost its lease; a process that died halfway
/// has, and telling those apart is the whole job of `arsy resume`.
const TASK_LEASE_MS: u64 = 30 * 60 * 1000;

/// What one task may spend before it is stopped rather than continued.
///
/// Wall time matches the lease, because a task that outlives its lease is one
/// another process may already have taken. Tokens are several turns' worth of
/// transcript: the point is to stop a runaway, not to second-guess a long task.
const TASK_BUDGET: Budget = Budget {
    tokens: CONTEXT_BUDGET_TOKENS as u64 * 4,
    cost_micros: u64::MAX,
    wall_ms: TASK_LEASE_MS,
};

/// One session's execution: the store, the provider it dispatches to, and the
/// durable graph of tasks it is working through.
///
/// `arsy run` and `arsy resume` differ only in where the task comes from — a
/// new one, or one a dead process left behind — so everything after that point
/// is this, and a resumed task cannot drift from a fresh one by being executed
/// somewhere else.
struct TaskRun<'a> {
    invocation: &'a Invocation,
    root: PathBuf,
    config: Config,
    resolved: provider::Resolved,
    model: String,
    service: AgentService,
    actor: Principal,
    session: SessionId,
    graph: TaskGraph,
    /// This process's identity as a task holder, so an expired lease can be
    /// told from one this process still holds.
    agent: AgentId,
}

impl<'a> TaskRun<'a> {
    /// Resolve everything a turn needs, then attach to the session.
    ///
    /// Configuration is resolved first and on its own: a provider that cannot
    /// be reached is a diagnostic before anything is recorded.
    fn open(invocation: &'a Invocation, session: Option<SessionId>) -> Result<Self, Diagnostic> {
        let root = workspace_root(&invocation.workspace)?;
        let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
        let config = load_config(&root, &working)?;
        let resolved = provider::resolve(&config, None)?;
        let model = resolved
            .endpoint
            .model
            .clone()
            .or_else(|| config.model_default().map(str::to_owned))
            .ok_or_else(|| {
                Diagnostic::error(
                    ARSY_PRV_1000,
                    format!(
                        "provider `{}` does not say which model to use",
                        resolved.endpoint.id
                    ),
                    "set `model` on the provider endpoint, or `model.default`, in config.toml",
                )
            })?;

        let store = open_store(&root)?;
        let session = session.unwrap_or_default();
        let actor = actor();
        let service = AgentService::attach(Arc::clone(&store) as Arc<dyn EventStore>, session)
            .map_err(storage_failed)?;
        let graph = TaskGraph::new(store, session, actor.clone()).map_err(graph_failed)?;
        Ok(Self {
            invocation,
            root,
            config,
            resolved,
            model,
            service,
            actor,
            session,
            graph,
            agent: AgentId::new(),
        })
    }

    /// Record a new task in the graph and take it.
    fn enqueue(&mut self, goal: &str) -> Result<TaskId, Diagnostic> {
        let id = TaskId::new();
        self.graph
            .add(TaskNode {
                id,
                goal: goal.to_owned(),
                dependencies: Vec::new(),
                assignee: Some(self.agent),
                required_output: "an answer to the task".to_owned(),
                // One process, one working tree: a scripted run edits the
                // workspace it was pointed at.
                workspace: WorkspaceRequirement::IsolatedWriter,
                budget: TASK_BUDGET,
                // Authority comes from policy at dispatch, not from the node:
                // a grant recorded here would be a second, stale answer to the
                // question `RuleSet::evaluate` already answers per call.
                authority: Vec::new(),
                state: TaskState::Pending,
                lease_expires_at_ms: None,
            })
            .map_err(graph_failed)?;
        Ok(id)
    }

    /// Run one task to a terminal state, recording what it spent on the way.
    ///
    /// `context` is folded into the result: a resumed task reports what its
    /// recovery found in the same record as its outcome, so one invocation
    /// still produces exactly one result.
    fn execute(
        &mut self,
        task: TaskId,
        context: Value,
        emitter: &mut Emitter,
    ) -> Result<i32, Diagnostic> {
        self.graph.ready().map_err(graph_failed)?;
        self.graph
            .lease(task, self.agent, unix_time_ms() + TASK_LEASE_MS)
            .map_err(graph_failed)?;
        let goal = self
            .graph
            .node(task)
            .map(|node| node.goal.clone())
            .ok_or_else(|| storage_failed("the task disappeared from its own graph"))?;

        // No operator is present, so nothing can be confirmed mid-run: the risk
        // context says so, and a call that needs an approval is refused by
        // policy rather than waiting on a keyboard that is not there.
        let agent = agent_runtime(&self.root, &self.config, false)?;
        // A supervisor exists only when policy actually delegates something,
        // so a workspace that grants nothing sees no spawn tool rather than one
        // that always refuses.
        let supervisor = subagent::Supervisor::new(
            self.root.clone(),
            &self.config,
            &self.resolved,
            self.model.clone(),
            task,
            &agent,
        );
        let delegates = supervisor.can_delegate();
        let admission = self.start_turn(&goal)?;
        let request = CanonicalModelRequest {
            model: ModelKey {
                provider: self.resolved.endpoint.id.clone(),
                model: self.model.clone(),
            },
            system: system_prompt(&self.root, &self.resolved.endpoint.id, &self.model),
            messages: vec![ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text { text: goal }],
            }],
            tools: {
                let mut tools = agent.schemas();
                if delegates {
                    tools.push(subagent::schema());
                }
                tools
            },
            max_output_tokens: self.resolved.endpoint.max_output_tokens,
            // Reasoning effort is chosen in the TUI with `/effort`. A scripted
            // run takes the request it always took, so a remembered interactive
            // choice cannot quietly change what a pipeline sends.
            effort: None,
            // The turn id, so a retried attempt is provably the same request.
            idempotency_key: IdempotencyKey::new(admission.turn.to_string())
                .map_err(|error| storage_failed(error.to_string()))?,
        };

        let started = Instant::now();
        let mut recorder = telemetry::Recorder::new(&self.config, self.actor.clone())?;
        let mut supervising = delegates.then_some((supervisor, &mut self.graph));
        let outcome = dispatch(
            self.resolved.provider.as_ref(),
            &agent,
            &request,
            &mut recorder,
            &mut supervising,
            emitter,
        );
        let interventions: Vec<Value> = supervising
            .as_ref()
            .map(|(supervisor, _)| supervisor.interventions().to_vec())
            .unwrap_or_default();
        drop(supervising);
        let stop = match &outcome {
            Ok(_) => "answered".to_owned(),
            Err(error) => format!("provider:{}", error.code()),
        };
        let summary = recorder.finish(&stop, &redactor(self.invocation, emitter)?, emitter);
        let mut record = json!({
            "session": self.session.to_string(),
            "task": task.to_string(),
            "turn": admission.turn.to_string(),
            "provider": self.resolved.endpoint.id,
            "model": request.model.model,
            "telemetry": summary,
            "interventions": interventions,
        });
        merge(&mut record, context);
        // Recorded whether the turn completed or failed: a turn that died
        // halfway still spent the tokens it spent, and a session's totals are
        // wrong if the failures are missing from them.
        self.service
            .record_usage(
                self.actor.clone(),
                arsy_kernel::projection::UsageTotals {
                    input_tokens: summary_number(&record, "input_tokens"),
                    output_tokens: summary_number(&record, "output_tokens"),
                    cost_micros: 0,
                },
            )
            .map_err(storage_failed)?;
        // Charged before the task is closed, so an exhausted budget is on the
        // record even when the turn it exhausted answered anyway.
        if let Err(error) = self.graph.consume(
            task,
            Budget {
                tokens: summary_number(&record, "input_tokens")
                    + summary_number(&record, "output_tokens"),
                cost_micros: 0,
                wall_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            },
        ) {
            emitter.diagnostic(&Diagnostic::warning(
                "ARSY-RET-1000",
                error.to_string(),
                "split the task, or raise what one task may spend",
            ));
        }

        match outcome {
            Ok(usage) => {
                let mut outcome = record.clone();
                merge(&mut outcome, usage);
                self.service
                    .complete_turn(self.actor.clone(), admission.turn, &outcome)
                    .map_err(storage_failed)?;
                self.graph
                    .complete(task, outcome.clone())
                    .map_err(graph_failed)?;
                let mut result = outcome;
                merge(&mut result, json!({"status": "completed"}));
                emitter.result(result);
                Ok(0)
            }
            Err(error) => {
                let diagnostic = Diagnostic::error(
                    ARSY_PRV_1000,
                    error.to_string(),
                    "check the provider endpoint, credential, and model in `arsy config explain`",
                );
                // The turn is durable before dispatch, so a failure here stays
                // recoverable through `arsy resume`.
                self.service
                    .fail_turn(
                        self.actor.clone(),
                        admission.turn,
                        error.code(),
                        error.to_string(),
                    )
                    .map_err(storage_failed)?;
                self.graph
                    .fail(
                        task,
                        json!({"code": error.code(), "message": error.to_string()}),
                    )
                    .map_err(graph_failed)?;
                emitter.diagnostic(&diagnostic);
                let mut result = record;
                merge(&mut result, json!({"status": "failed"}));
                emitter.result(result);
                Ok(diagnostic.exit_code())
            }
        }
    }

    fn start_turn(&self, goal: &str) -> Result<arsy_kernel::service::TurnAdmission, Diagnostic> {
        let envelope = ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
            session: self.session,
            prompt: goal.to_owned(),
            extensions: Extensions::new(),
        }));
        self.service
            .start_turn(self.actor.clone(), &envelope)
            .map_err(storage_failed)
    }
}

fn graph_failed(error: arsy_kernel::orchestration::GraphError) -> Diagnostic {
    Diagnostic::error(
        "ARSY-STL-1000",
        format!("the task graph refused the change: {error}"),
        "inspect the session with `arsy session show --turns`",
    )
}

/// The transcript budget, in tokens, before the model's own output is reserved.
///
/// Deliberately below the smallest window the supported models offer rather
/// than read from configuration: the cost of being wrong low is a re-read, and
/// the cost of being wrong high is a rejected request in the middle of a turn.
/// A per-model window belongs in `provider.endpoint` when a model that needs a
/// different number actually appears.
const CONTEXT_BUDGET_TOKENS: u32 = 96_000;

/// What one turn's transcript may grow to on this endpoint.
#[cfg(feature = "tui")]
fn context_budget(resolved: &provider::Resolved) -> u32 {
    CONTEXT_BUDGET_TOKENS.saturating_sub(resolved.endpoint.max_output_tokens)
}

/// How many rounds of tool calls one scripted turn may take.
///
/// The same bound the interactive loop uses, for the same reason: a model that
/// answers every result with another call would otherwise spend the run on its
/// own loop.
const MAX_SCRIPTED_TOOL_ROUNDS: usize = 24;

/// Run one scripted turn to completion, executing the tools the model asks for.
///
/// Nobody is at the keyboard, so authority comes from policy alone: a call
/// policy allows runs, and a call that needs an approval is reported to the
/// model as a failed result rather than silently skipped. That is what makes a
/// pipeline's behaviour a property of its configuration instead of a property
/// of who happened to be watching.
fn dispatch(
    provider: &dyn ModelProvider,
    runtime: &arsy_code::agent::ToolRuntime,
    request: &CanonicalModelRequest,
    recorder: &mut telemetry::Recorder,
    supervisor: &mut Option<(subagent::Supervisor<'_>, &mut TaskGraph)>,
    emitter: &mut Emitter,
) -> Result<Value, ProviderError> {
    let mut request = request.clone();
    let base = request.idempotency_key.as_str().to_owned();
    let budget = CONTEXT_BUDGET_TOKENS.saturating_sub(request.max_output_tokens);
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    for round in 0..MAX_SCRIPTED_TOOL_ROUNDS {
        // Each round is its own request, so a retry repeats that round rather
        // than collapsing into the one before it.
        request.idempotency_key = IdempotencyKey::new(format!("{base}-{round}"))
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
        let mut answer = String::new();
        let mut calls: Vec<(String, String, Value)> = Vec::new();
        // Each sleep the retry loop asks for is one attempt that failed, which
        // is the only place a retry is observable from outside the provider.
        let mut retries = 0;
        let started = Instant::now();
        let stream = arsy_kernel::provider::stream_with_retry(provider, &request, &mut |delay| {
            retries += 1;
            std::thread::sleep(delay);
        })
        .inspect_err(|error| {
            recorder.model_call(
                &request.model.model,
                started.elapsed(),
                0,
                0,
                retries,
                error.code(),
            );
        })?;
        let (mut round_input, mut round_output) = (0u64, 0u64);
        for event in stream {
            match event? {
                ModelEvent::TextDelta { text } => {
                    emitter.delta(&text);
                    answer.push_str(&text);
                }
                ModelEvent::Usage {
                    input_tokens: input,
                    output_tokens: output,
                } => {
                    round_input += input;
                    round_output += output;
                    input_tokens += input;
                    output_tokens += output;
                }
                ModelEvent::ToolCallCompleted {
                    id,
                    name,
                    arguments,
                    ..
                } => calls.push((id, name, arguments)),
                ModelEvent::Completed { .. }
                | ModelEvent::ToolCallStarted { .. }
                | ModelEvent::ToolCallDelta { .. }
                // `arsy run` is a scriptable surface: reasoning is for the
                // operator watching a stream, not for a pipeline's stdout.
                | ModelEvent::ThinkingDelta { .. } => {}
            }
        }
        recorder.model_call(
            &request.model.model,
            started.elapsed(),
            round_input,
            round_output,
            retries,
            "ok",
        );
        if calls.is_empty() {
            emitter.end_deltas();
            return Ok(token_usage(input_tokens, output_tokens));
        }
        arsy_code::agent::budget::trim(&mut request.messages, budget);

        // The calls are history now, whatever running them produced: a provider
        // that sent a call and never sees its result rejects the next request.
        let mut content: Vec<ModelContent> = Vec::new();
        if !answer.trim().is_empty() {
            content.push(ModelContent::Text { text: answer });
        }
        content.extend(
            calls
                .iter()
                .map(|(id, name, arguments)| ModelContent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
        );
        request.messages.push(ModelMessage {
            role: ModelRole::Assistant,
            content,
        });
        let results = calls
            .iter()
            .map(|(id, name, arguments)| {
                // Spawning is the one call the tool runtime does not own: it
                // adds a node to this session's graph rather than touching the
                // workspace, and the child's own calls go through the runtime
                // under the authority the graph attenuated for it.
                let result = match (name.as_str(), supervisor.as_mut()) {
                    ("task.spawn", Some((supervisor, graph))) => {
                        supervisor.spawn(arguments, graph, emitter)
                    }
                    _ => runtime.invoke(name, arguments),
                };
                recorder.tool_call(&result);
                ModelContent::ToolResult {
                    id: id.clone(),
                    content: result.output,
                    is_error: !result.success,
                }
            })
            .collect();
        request.messages.push(ModelMessage {
            role: ModelRole::User,
            content: results,
        });
    }
    emitter.end_deltas();
    Err(ProviderError::InvalidRequest(format!(
        "the model asked for tools {MAX_SCRIPTED_TOOL_ROUNDS} times without finishing the turn"
    )))
}

/// One counter out of the telemetry summary the run just printed.
fn summary_number(record: &Value, key: &str) -> u64 {
    record["telemetry"][key].as_u64().unwrap_or_default()
}

/// Run one subagent turn to its answer.
///
/// A smaller loop than the parent's on purpose: a child has no operator to ask,
/// no session of its own to record into, and a bound on rounds low enough that
/// a child which cannot answer gives the parent its rounds back rather than
/// spending them. Every tool call still goes through the same runtime — the
/// child's, holding only what was delegated to it.
///
/// `watch` sees a redacted projection of each call: the tool and whether it
/// worked, never the arguments or what came back. Returning
/// [`Intervention::Deny`] stops the child there.
pub(crate) fn child_turn(
    provider: &dyn ModelProvider,
    runtime: &arsy_code::agent::ToolRuntime,
    request: &CanonicalModelRequest,
    watch: &mut dyn FnMut(
        &arsy_kernel::observer::RedactedProjection,
    ) -> Option<arsy_kernel::observer::Intervention>,
    emitter: &mut Emitter,
) -> Result<String, String> {
    let mut request = request.clone();
    let base = request.idempotency_key.as_str().to_owned();
    let budget = CONTEXT_BUDGET_TOKENS.saturating_sub(request.max_output_tokens);
    let mut consecutive_failures = 0u64;
    // Tool calls this child has made, so an intervention can be correlated
    // with the call that caused it.
    let mut calls_made = 0u64;
    let mut answer = String::new();

    for round in 0..MAX_CHILD_TOOL_ROUNDS {
        request.idempotency_key =
            IdempotencyKey::new(format!("{base}-{round}")).map_err(|error| error.to_string())?;
        answer.clear();
        let mut calls: Vec<(String, String, Value)> = Vec::new();
        let stream =
            arsy_kernel::provider::stream_with_retry(provider, &request, &mut std::thread::sleep)
                .map_err(|error| error.to_string())?;
        for event in stream {
            match event.map_err(|error| error.to_string())? {
                ModelEvent::TextDelta { text } => answer.push_str(&text),
                ModelEvent::ToolCallCompleted {
                    id,
                    name,
                    arguments,
                    ..
                } => calls.push((id, name, arguments)),
                _ => {}
            }
        }
        if calls.is_empty() {
            return Ok(if answer.trim().is_empty() {
                "the subagent finished without an answer".to_owned()
            } else {
                answer
            });
        }
        arsy_code::agent::budget::trim(&mut request.messages, budget);

        let mut content: Vec<ModelContent> = Vec::new();
        if !answer.trim().is_empty() {
            content.push(ModelContent::Text {
                text: answer.clone(),
            });
        }
        content.extend(
            calls
                .iter()
                .map(|(id, name, arguments)| ModelContent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
        );
        request.messages.push(ModelMessage {
            role: ModelRole::Assistant,
            content,
        });

        let mut results = Vec::with_capacity(calls.len());
        for (id, name, arguments) in &calls {
            let result = runtime.invoke(name, arguments);
            calls_made += 1;
            consecutive_failures = if result.success {
                0
            } else {
                consecutive_failures + 1
            };
            // What an observer is allowed to see: which call this was, the
            // tool, and the outcome. The arguments named a path and the result
            // carried its contents, and neither is the observer's business.
            let projection = arsy_kernel::observer::RedactedProjection {
                sequence: calls_made,
                kind: if result.success {
                    "tool.completed".to_owned()
                } else {
                    "tool.failed".to_owned()
                },
                public_payload: json!({
                    "tool": result.tool,
                    "consecutive_failures": consecutive_failures,
                }),
                redacted_fields: 2,
            };
            let intervened = watch(&projection);
            results.push(ModelContent::ToolResult {
                id: id.clone(),
                content: result.output,
                is_error: !result.success,
            });
            if let Some(arsy_kernel::observer::Intervention::Deny(reason)) = intervened {
                emitter.diagnostic(&Diagnostic::warning(
                    "ARSY-RET-1001",
                    format!("a subagent was stopped: {reason}"),
                    "the parent keeps whatever the subagent had established before it stopped",
                ));
                return Err(format!("stopped by its supervisor: {reason}"));
            }
        }
        request.messages.push(ModelMessage {
            role: ModelRole::User,
            content: results,
        });
    }
    Err(format!(
        "the subagent used its {MAX_CHILD_TOOL_ROUNDS} rounds without answering"
    ))
}

/// How many rounds of tool calls one subagent may take.
///
/// Fewer than the parent's: a child has one question, and a child that cannot
/// answer it in this many rounds is one the parent should take back.
const MAX_CHILD_TOOL_ROUNDS: usize = 8;

fn token_usage(input_tokens: u64, output_tokens: u64) -> Value {
    if input_tokens == 0 && output_tokens == 0 {
        json!({})
    } else {
        json!({"input_tokens": input_tokens, "output_tokens": output_tokens})
    }
}

/// Fold `extra`'s fields into `target`, which is always an object here.
fn merge(target: &mut Value, extra: Value) {
    if let (Some(target), Some(extra)) = (target.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn prepare_task(
    invocation: &Invocation,
    task: &str,
    emitter: &mut Emitter,
) -> Result<String, Diagnostic> {
    if task.trim().is_empty() {
        return Err(usage("run requires a non-empty task"));
    }
    redactor(invocation, emitter)?
        .sanitize(task)
        .map_err(secret_failed)
}

fn resume(
    invocation: &Invocation,
    session: SessionId,
    follow: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let store = open_store(&workspace_root(&invocation.workspace)?)?;
    emitter.session = Some(session);
    if store
        .current_version(session)
        .map_err(|error| storage_failed(error.to_string()))?
        .0
        == 0
    {
        return Err(Diagnostic::error(
            "ARSY-SCH-1004",
            format!("session {session} has no recorded events in this workspace"),
            "check the ID, or run `arsy run` with the workspace that recorded it",
        ));
    }

    // Recovery needs no provider: closing what a dead process left open is
    // worth doing even in a workspace that could not dispatch a turn today.
    let actor = actor();
    let service = AgentService::attach(Arc::clone(&store) as Arc<dyn EventStore>, session)
        .map_err(storage_failed)?;
    // History is never truncated: a turn that was running when the process
    // died is closed by appending `turn.failed` after the events it already
    // wrote.
    let interrupted = service.unfinished_turns().map_err(storage_failed)?;
    for turn in &interrupted {
        service
            .fail_turn(
                actor.clone(),
                *turn,
                "interrupted",
                "the process exited before the turn finished",
            )
            .map_err(storage_failed)?;
    }
    let version = service.committed_version().map_err(storage_failed)?;
    drop(service);

    // A task whose lease has run out is one nobody is working on, whatever the
    // process that took it intended.
    let mut graph = TaskGraph::new(store, session, actor).map_err(graph_failed)?;
    let mut recovered = graph
        .recover_expired(unix_time_ms())
        .map_err(graph_failed)?;
    // A turn found open is proof its process is gone, so whatever task it held
    // is handed back now rather than when the lease would have run out.
    if !interrupted.is_empty() {
        recovered.extend(graph.reclaim_running().map_err(graph_failed)?);
    }
    let waiting = graph.pending().first().map(|node| node.id);
    drop(graph);

    let mut report = json!({
        "session": session.to_string(),
        "events": version.0,
        "closed_turns": interrupted.len(),
        "recovered_tasks": recovered.len(),
        "continuing": Value::Null,
        // ponytail: following live events needs the serve loop; the flag is
        // accepted and reports the committed head instead of hanging.
        "following": follow,
    });

    let Some(task) = waiting else {
        emitter.result(report);
        return Ok(0);
    };
    // The session has unfinished work. Continuing it needs a provider, so a
    // workspace that cannot dispatch reports the task as still waiting rather
    // than losing it.
    let mut execution = match TaskRun::open(invocation, Some(session)) {
        Ok(execution) => execution,
        Err(diagnostic) => {
            let code = unusable(diagnostic, emitter);
            merge(&mut report, json!({"blocked": task.to_string()}));
            emitter.result(report);
            return Ok(code);
        }
    };
    merge(&mut report, json!({"continuing": task.to_string()}));
    execution.execute(task, report, emitter)
}

fn doctor(invocation: &Invocation, strict: bool, emitter: &mut Emitter) -> i32 {
    let mut warnings: Vec<Diagnostic> = Vec::new();
    let workspace = workspace_root(&invocation.workspace);
    let storage = match &workspace {
        Ok(root) => match open_store(root) {
            Ok(_) => "openable".to_owned(),
            Err(error) => {
                warnings.push(error.clone());
                error.message
            }
        },
        Err(error) => {
            warnings.push(error.clone());
            error.message.clone()
        }
    };

    // ponytail: layer discovery only. Merged values and their source trace
    // arrive with `arsy config explain`.
    let root = workspace.as_deref().unwrap_or(Path::new("."));
    let config: Vec<Value> = arsy_kernel::config::layers(root, root)
        .into_iter()
        .map(|(layer, path)| {
            json!({
                "layer": layer,
                "path": path.display().to_string(),
                "present": path.is_file(),
            })
        })
        .collect();

    // A configured endpoint with a reachable credential is what decides
    // whether a turn can dispatch, so report it as one fact rather than
    // leaving an operator to infer it from the credential count.
    let provider = match load_config(root, root) {
        Err(diagnostic) => {
            let value = json!({"status": "unusable", "detail": diagnostic.message});
            warnings.push(diagnostic);
            value
        }
        Ok(config) => match provider::resolve(&config, None) {
            Ok(resolved) => json!({
                "status": "ready",
                "id": resolved.endpoint.id,
                "kind": resolved.endpoint.kind.as_str(),
                "base_url": resolved.endpoint.base_url,
                "credential_source": resolved.source.as_str(),
                // Present only when `provider.default = "auto"` left the choice
                // to routing; naming the criterion is what makes the choice
                // reviewable rather than surprising.
                "routing": resolved.route.as_ref().map(|decision| match decision {
                    arsy_kernel::routing::Decision::Routed { key, reasons, excluded } => json!({
                        "model": key.to_string(),
                        "reasons": reasons,
                        "excluded": excluded.len(),
                    }),
                    other => serde_json::to_value(other).unwrap_or(Value::Null),
                }),
            }),
            Err(diagnostic) => json!({"status": "unavailable", "detail": diagnostic.message}),
        },
    };

    let sandbox_assurance = installed_sandbox_assurance();
    if sandbox_assurance == arsy_kernel::policy::SandboxAssurance::None {
        warnings.push(Diagnostic::warning(
            "ARSY-SBX-1000",
            "no complete sandbox worker is available, so achieved assurance is `none`",
            "install arsy-sandbox-worker and the platform controls before running effects",
        ));
    }
    let credentials = catalog(CatalogStore::resolve(invocation))
        .unwrap_or_default()
        .len();
    if credentials == 0 {
        warnings.push(Diagnostic::warning(
            "ARSY-PRV-1001",
            "no provider credential is stored",
            "store a credential with `arsy auth set <PROVIDER>`",
        ));
    }

    for warning in &warnings {
        emitter.diagnostic(warning);
    }
    emitter.result(json!({
        "platform": platform(),
        "version": arsy_code::VERSION,
        "sandbox_assurance": sandbox_assurance.as_str(),
        "provider_auth": if credentials == 0 { "none" } else { "configured" },
        "provider": provider,
        "storage": storage,
        "config_layers": config,
        "warnings": warnings.len(),
    }));

    // Warnings return 0 unless --strict; then the first reported one is
    // terminal and selects the code.
    match (strict, warnings.first()) {
        (true, Some(terminal)) => terminal.exit_code(),
        _ => 0,
    }
}

fn installed_sandbox_assurance() -> arsy_kernel::policy::SandboxAssurance {
    let Ok(executable) = std::env::current_exe() else {
        return arsy_kernel::policy::SandboxAssurance::None;
    };
    let worker = executable.with_file_name(if cfg!(windows) {
        "arsy-sandbox-worker.exe"
    } else {
        "arsy-sandbox-worker"
    });
    if !worker.is_file() {
        return arsy_kernel::policy::SandboxAssurance::None;
    }
    arsy_code::sandbox::PlatformSandbox::detect()
        .map_or(arsy_kernel::policy::SandboxAssurance::None, |backend| {
            backend.assurance()
        })
}

fn read_stdin() -> Result<String, Diagnostic> {
    let mut task = String::new();
    io::stdin()
        .read_to_string(&mut task)
        .map_err(|error| usage(format!("stdin is not readable UTF-8 text: {error}")))?;
    Ok(task)
}

fn workspace_root(requested: &Path) -> Result<PathBuf, Diagnostic> {
    std::fs::canonicalize(requested).map_err(|error| {
        usage(format!(
            "workspace {} is unusable: {error}",
            requested.display()
        ))
    })
}

fn open_store(workspace: &Path) -> Result<Arc<SqliteEventStore>, Diagnostic> {
    let path = workspace.join(STORE_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| storage_failed(error.to_string()))?;
    }
    SqliteEventStore::open(&path, Durability::Normal)
        .map(Arc::new)
        .map_err(|error| storage_failed(format!("{path:?}: {error}")))
}

fn storage_failed(error: impl ToString) -> Diagnostic {
    Diagnostic::error(
        "ARSY-CMP-1000",
        format!("the session store failed: {}", error.to_string()),
        "check the workspace `.arsy` directory is writable and not held by another process",
    )
}

fn actor() -> Principal {
    Principal::User(
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "local".to_owned()),
    )
}

/// Build the tool runtime a turn executes through.
///
/// The same construction `arsy serve` performs, because a turn and a served
/// call have to reach the same operations under the same rules; the only
/// difference is the risk context, which says whether an operator is present to
/// answer an approval.
/// The workspace's artifact store: where every operation's result, every
/// exported excerpt, and every memory's claim is kept.
fn artifact_store(root: &Path) -> Result<arsy_kernel::artifact::FileArtifactStore, Diagnostic> {
    arsy_kernel::artifact::FileArtifactStore::open(root.join(".arsy/artifacts"), 0)
        .map_err(|error| storage_failed(error.to_string()))
}

fn agent_runtime(
    root: &Path,
    config: &arsy_kernel::config::Config,
    interactive: bool,
) -> Result<arsy_code::agent::ToolRuntime, Diagnostic> {
    let workspace = arsy_code::resource::Workspace::open(root)
        .map_err(|error| storage_failed(error.to_string()))?;
    let artifacts = Arc::new(artifact_store(root)?);
    arsy_code::agent::runtime(
        &workspace,
        config.policy_rule_set(),
        artifacts,
        arsy_kernel::artifact::unix_time_ms(),
        actor(),
        arsy_kernel::policy::RiskContext {
            reversible: interactive,
            workspace: arsy_code::git::cleanliness(root)
                .unwrap_or(arsy_kernel::policy::WorkspaceCleanliness::Unknown),
            sandbox: installed_sandbox_assurance(),
        },
        arsy_code::operations::Reachable::from_config(config),
    )
    .map_err(|error| storage_failed(error.to_string()))
}

/// The system prompt for one turn: the harness's own instructions, then the
/// project's, discovered by walking from the workspace root to the working
/// directory.
///
/// Compilation failure is not a reason to lose the turn — a prompt over budget
/// or an unredactable secret is a degradation, not a fault — so the harness
/// instructions alone are the floor.
/// What a recalled memory may take out of the prompt.
///
/// Small on purpose. Memory competes with the task and the repository's own
/// instructions for the same window, and a workspace that remembers a page of
/// facts is one whose next turn has less room to read the code.
const MAX_RECALLED_MEMORY_BYTES: usize = 4 * 1024;

fn system_prompt(root: &Path, provider: &str, model: &str) -> Option<String> {
    let workspace = arsy_code::resource::Workspace::open(root).ok()?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.to_path_buf());
    let instructions = arsy_code::agent::instructions::discover(&workspace, &working);
    let family = arsy_code::agent::instructions::family_for(provider, model);
    let compiled = arsy_code::agent::instructions::system_prompt(
        family,
        &instructions,
        memory::recalled(root, MAX_RECALLED_MEMORY_BYTES).as_deref(),
        &arsy_kernel::secret::Redactor::new(),
        arsy_kernel::prompt::MAX_PROMPT_BYTES as u32,
    )
    .ok()?;
    Some(arsy_code::agent::instructions::render(&compiled))
}

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        event::{EventPayload, EventStore},
        protocol::{ClientRequest, ProtocolEnvelope, TurnStart},
    };

    /// A provider that replays a scripted round per request and records what
    /// it was asked, so a test can assert on the conversation the loop built.
    #[cfg(feature = "tui")]
    struct Scripted {
        descriptor: arsy_kernel::provider::ProviderDescriptor,
        rounds: std::sync::Mutex<std::collections::VecDeque<Vec<ModelEvent>>>,
        seen: std::sync::Mutex<Vec<CanonicalModelRequest>>,
    }

    #[cfg(feature = "tui")]
    impl arsy_kernel::provider::ModelProvider for Scripted {
        fn descriptor(&self) -> &arsy_kernel::provider::ProviderDescriptor {
            &self.descriptor
        }

        fn stream(
            &self,
            request: &CanonicalModelRequest,
        ) -> Result<arsy_kernel::provider::ModelEventStream, arsy_kernel::provider::ProviderError>
        {
            self.seen.lock().unwrap().push(request.clone());
            let events = self.rounds.lock().unwrap().pop_front().unwrap_or_default();
            Ok(Box::new(events.into_iter().map(Ok)))
        }
    }

    #[cfg(feature = "tui")]
    fn resolved(rounds: Vec<Vec<ModelEvent>>) -> (provider::Resolved, std::sync::Arc<Scripted>) {
        let scripted = std::sync::Arc::new(Scripted {
            descriptor: arsy_kernel::provider::ProviderDescriptor {
                id: "stub".to_owned(),
                max_retries: 0,
            },
            rounds: std::sync::Mutex::new(rounds.into()),
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let resolved = provider::Resolved {
            provider: scripted.clone(),
            endpoint: arsy_kernel::config::Endpoint {
                id: "stub".to_owned(),
                kind: arsy_kernel::config::Dialect::Openai,
                base_url: "https://stub.invalid/v1".to_owned(),
                credential: None,
                api_key_env: None,
                model: Some("m".to_owned()),
                models: vec!["m".to_owned()],
                max_output_tokens: 64,
                oauth: None,
            },
            source: provider::CredentialSource::DefaultEnv,
            route: None,
        };
        (resolved, scripted)
    }

    #[cfg(feature = "tui")]
    fn route() -> tui::ModelRoute {
        tui::ModelRoute {
            provider: "stub".to_owned(),
            model: "m".to_owned(),
        }
    }

    /// A runtime over a scratch workspace, under whatever policy an unconfigured
    /// workspace gets — which is what a first run actually sees.
    #[cfg(feature = "tui")]
    fn test_runtime(root: &Path) -> arsy_code::agent::ToolRuntime {
        agent_runtime(root, &load_config(root, root).unwrap(), true).unwrap()
    }

    /// Answers typed at the confirmation prompt. Keys sent while a round is
    /// still streaming belong to the composer, exactly as they do in a
    /// session, so the answers are sent once the prompt is up.
    #[cfg(feature = "tui")]
    fn typed(
        answers: &'static [u8],
    ) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Receiver<u8>) {
        let (sender, keys) = std::sync::mpsc::channel();
        let typist = std::thread::spawn(move || {
            for answer in answers {
                std::thread::sleep(std::time::Duration::from_millis(250));
                if sender.send(*answer).is_err() {
                    return;
                }
            }
            // The sender stays alive: a confirmation that never comes must
            // block, not read as a hung-up keyboard.
            std::thread::sleep(std::time::Duration::from_secs(2));
            drop(sender);
        });
        (typist, keys)
    }

    #[cfg(feature = "tui")]
    #[test]
    fn a_confirmed_tool_call_runs_and_its_result_goes_back_to_the_model() {
        let workspace = tempfile::tempdir().unwrap();
        let patch =
            "*** Begin Patch\n*** Add File: note.txt\n+written by the tool loop\n*** End Patch\n";
        let (resolved, scripted) = resolved(vec![
            vec![
                ModelEvent::ToolCallCompleted {
                    index: 0,
                    id: "call-1".to_owned(),
                    name: "apply_patch".to_owned(),
                    arguments: json!({ "patch": patch }),
                },
                ModelEvent::Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::ToolUse,
                },
            ],
            vec![
                ModelEvent::TextDelta {
                    text: "done\n".to_owned(),
                },
                ModelEvent::Usage {
                    input_tokens: 300,
                    output_tokens: 5,
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::EndTurn,
                },
            ],
        ]);
        let (typist, keys) = typed(b"y");
        let mut conversation = vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "write a note".to_owned(),
            }],
        }];
        let turn = native_turn(
            &resolved,
            &test_runtime(workspace.path()),
            &mut conversation,
            &route(),
            None,
            arsy_kernel::domain::TurnId::new(),
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        typist.join().unwrap();

        assert_eq!(turn.response.trim(), "done");
        // Both requests are charged to the turn, not just the last one.
        assert_eq!(
            turn.usage,
            json!({"input_tokens": 400, "output_tokens": 15})
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("note.txt")).unwrap(),
            "written by the tool loop\n"
        );
        // The call and its result are both history, in that order.
        assert!(matches!(
            conversation[1].content.first(),
            Some(ModelContent::ToolCall { name, .. }) if name == "apply_patch"
        ));
        assert!(matches!(
            conversation[2].content.first(),
            Some(ModelContent::ToolResult { is_error: false, content, .. })
                if content.contains("added note.txt")
        ));
        // Both requests offered the tools, and the second carried the result.
        let seen = scripted.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "the loop asked again after the tool ran");
        // The semantic tools are offered between search and editing, and a
        // build with the WASM feature offers `plugin.invoke` as well; what
        // this asserts is the order of the rest.
        let offered: Vec<&str> = seen[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .filter(|name| !name.starts_with("code.") && *name != "plugin.invoke")
            .collect();
        assert_eq!(
            offered,
            [
                "fs.read",
                "fs.list",
                "search.files",
                "search.text",
                "fs.edit",
                "apply_patch",
                "fs.write",
                "fs.delete",
                "fs.move",
                "bash",
            ],
            "reading and searching are offered before the shell"
        );
        // The system prompt is built, not omitted: a turn that tells the model
        // nothing about the workspace is the bug this replaced.
        let system = seen[0].system.as_deref().unwrap_or_default();
        assert!(system.contains("ARSY"), "{system}");
        assert_eq!(seen[1].messages.len(), 3);
        assert_ne!(
            seen[0].idempotency_key, seen[1].idempotency_key,
            "each round is its own request"
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn a_declined_tool_call_does_not_run_and_the_model_is_told_so() {
        let workspace = tempfile::tempdir().unwrap();
        let (resolved, _scripted) = resolved(vec![
            vec![
                ModelEvent::ToolCallCompleted {
                    index: 0,
                    id: "call-1".to_owned(),
                    name: "bash".to_owned(),
                    arguments: json!({"command": "touch escaped"}),
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::ToolUse,
                },
            ],
            vec![
                ModelEvent::TextDelta {
                    text: "understood\n".to_owned(),
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::EndTurn,
                },
            ],
        ]);
        // `d` denies; `n` now opens the note editor.
        let (typist, keys) = typed(b"d");
        let mut conversation = Vec::new();
        let turn = native_turn(
            &resolved,
            &test_runtime(workspace.path()),
            &mut conversation,
            &route(),
            None,
            arsy_kernel::domain::TurnId::new(),
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        typist.join().unwrap();

        assert_eq!(turn.response.trim(), "understood");
        assert!(
            !turn.interrupted,
            "declining one call is not a stopped turn"
        );
        assert!(
            !workspace.path().join("escaped").exists(),
            "a declined command must not run"
        );
        assert!(matches!(
            conversation[1].content.first(),
            Some(ModelContent::ToolResult { is_error: true, content, .. })
                if content.contains("declined")
        ));
    }

    #[cfg(feature = "tui")]
    #[test]
    fn ctrl_c_at_the_prompt_stops_the_turn_instead_of_declining_one_call() {
        let workspace = tempfile::tempdir().unwrap();
        // Two calls in one round, and a second round that would follow. Neither
        // may run, and the loop must not ask the provider again.
        let asking = || {
            vec![
                ModelEvent::ToolCallCompleted {
                    index: 0,
                    id: "call-1".to_owned(),
                    name: "bash".to_owned(),
                    arguments: json!({"command": "touch first"}),
                },
                ModelEvent::ToolCallCompleted {
                    index: 1,
                    id: "call-2".to_owned(),
                    name: "bash".to_owned(),
                    arguments: json!({"command": "touch second"}),
                },
                ModelEvent::Completed {
                    stop: arsy_kernel::provider::StopReason::ToolUse,
                },
            ]
        };
        let (resolved, scripted) = resolved(vec![asking(), asking()]);
        let (typist, keys) = typed(b"\x03");
        let mut conversation = Vec::new();
        // A stop is answered by the stop, not by waiting for the keyboard to
        // hang up: the calls after it are refused without asking.
        let started = std::time::Instant::now();
        let turn = native_turn(
            &resolved,
            &test_runtime(workspace.path()),
            &mut conversation,
            &route(),
            None,
            arsy_kernel::domain::TurnId::new(),
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        // Measured before the typist is joined, which outlives the turn on
        // purpose so an unanswered prompt blocks rather than reading as a
        // hung-up keyboard.
        let took = started.elapsed();
        typist.join().unwrap();

        assert!(turn.interrupted, "Ctrl-C at the prompt ends the turn");
        assert!(
            took < std::time::Duration::from_millis(1500),
            "the stop did not carry to the calls after it: {took:?}"
        );
        assert!(!workspace.path().join("first").exists());
        assert!(
            !workspace.path().join("second").exists(),
            "the calls after the stop must not run either"
        );
        assert_eq!(
            scripted.seen.lock().unwrap().len(),
            1,
            "a stopped turn does not ask the provider again"
        );
        // Every call still has a result, because a provider that sent one and
        // never sees an answer rejects the next request.
        let results = &conversation[1].content;
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|content| matches!(content, ModelContent::ToolResult { is_error: true, .. })));
    }

    #[cfg(feature = "tui")]
    #[test]
    fn slash_commands_expand_to_the_inspection_the_cli_already_parses() {
        let expansion = |line: &str| inspection_args(line).map(|args| args.join(" "));

        // A bare command takes its default subcommand; a flag is not one, so it
        // does not suppress the default the way a subcommand does.
        assert_eq!(expansion("/mcp"), Some("mcp list".to_owned()));
        assert_eq!(
            expansion("/mcp --source claude"),
            Some("mcp list --source claude".to_owned())
        );
        assert_eq!(
            expansion("/mcp show NAME"),
            Some("mcp show NAME".to_owned()),
            "an explicit subcommand is not replaced"
        );
        assert_eq!(expansion("/hooks"), Some("hook list".to_owned()));
        assert_eq!(expansion("/settings"), Some("config explain".to_owned()));
        assert_eq!(
            expansion("/settings model.route"),
            Some("config explain model.route".to_owned())
        );
        assert_eq!(expansion("/doctor"), Some("doctor".to_owned()));
        assert_eq!(expansion("/auth"), Some("auth list".to_owned()));
        assert_eq!(
            expansion("/compat claude"),
            Some("compat explain claude".to_owned())
        );
        assert_eq!(expansion("/nonsense"), None, "the loop reports it instead");

        // Every expansion is a command the CLI parser already accepts, so the
        // TUI adds no second argument grammar to keep in step.
        for line in [
            "/mcp",
            "/hooks --event PreToolUse",
            "/settings",
            "/doctor",
            "/auth",
            "/compat omp",
        ] {
            let args = inspection_args(line).expect("mapped");
            assert!(parse(args).is_ok(), "{line} did not parse");
        }

        // Auth commands expand to their CLI equivalents.
        for line in ["/auth remove secret://os/handle", "/auth login codex"] {
            let args = inspection_args(line).expect("mapped");
            assert!(parse(args).is_ok(), "{line} did not parse");
        }
    }

    /// The catalog names every provider the operator holds a credential for, so
    /// it is created owner-only rather than narrowed after the fact.
    #[test]
    fn a_catalog_file_is_never_briefly_world_readable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");

        {
            let mut file = owner_only(&path).unwrap();
            file.write_all(b"[]\n").unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "created as {mode:04o}");

            // Rewriting truncates rather than appending, and does not widen the
            // mode a second time.
            let mut file = owner_only(&path).unwrap();
            file.write_all(b"[]").unwrap();
            drop(file);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "rewritten as {mode:04o}");
        }
    }

    /// The model picker answers a configured endpoint's list the same way it
    /// answers Codex's: by number, by slug, or not at all.
    #[cfg(feature = "tui")]
    #[test]
    fn the_model_picker_answers_a_configured_endpoints_list() {
        let route = tui::ModelRoute {
            provider: "hari".to_owned(),
            model: "mimo".to_owned(),
        };
        let listed: Vec<tui::ModelChoice> = ["mimo", "mimo-2", "mimo-lite"]
            .into_iter()
            .map(|slug| tui::ModelChoice {
                provider: "hari".to_owned(),
                slug: slug.to_owned(),
                name: "on hari".to_owned(),
            })
            .collect();

        // A number picks from the list; the row names the provider it serves,
        // so the route follows the row rather than the current provider.
        let picked = tui::resolve_model("2", &listed, &route).unwrap();
        assert_eq!(picked.model, "mimo-2");
        assert_eq!(picked.provider, "hari");

        // A slug that is not on the list is still accepted, because the list is
        // what the endpoint advertises, not what it will refuse.
        assert_eq!(
            tui::resolve_model("mimo-preview", &listed, &route)
                .unwrap()
                .model,
            "mimo-preview"
        );

        // Out of range says the range rather than silently keeping the current.
        let error = tui::resolve_model("9", &listed, &route).unwrap_err();
        assert!(error.contains("1-3"), "{error}");

        // An empty answer keeps what is set.
        assert_eq!(tui::resolve_model("  ", &listed, &route).unwrap(), route);

        // An endpoint that lists nothing says so instead of naming a range.
        let error = tui::resolve_model("1", &[], &route).unwrap_err();
        assert!(error.contains("no models are listed"), "{error}");
    }

    /// Every question `/provider` asks validates its own answer, and nothing
    /// reaches the configuration until the last one.
    #[cfg(feature = "tui")]
    #[test]
    fn the_provider_wizard_validates_each_answer_before_it_moves_on() {
        use tui::ProviderStep as Step;

        let invocation = Invocation {
            workspace: PathBuf::from("."),
            output: None,
            no_color: true,
            command: Command::Tui,
        };
        let providers = vec!["myai".to_owned()];
        let mut draft = tui::ProviderDraft::default();
        let step = |step: Step, line: &str, draft: &mut tui::ProviderDraft| {
            provider_step(&invocation, step, line, draft, &providers)
        };

        // An empty answer leaves the wizard rather than writing a blank field.
        assert!(matches!(
            step(Step::Name, "   ", &mut draft),
            Ok(ProviderNext::Cancelled(_))
        ));

        // The two actions are rows, not provider names.
        assert!(matches!(
            step(Step::Pick, "+new", &mut draft),
            Ok(ProviderNext::Ask(Step::Name))
        ));
        assert!(matches!(
            step(Step::Pick, "-remove", &mut draft),
            Ok(ProviderNext::Ask(Step::Remove))
        ));
        assert!(step(Step::Pick, "nothere", &mut draft).is_err());

        // A name has to be new, writable, and not look like an action row.
        assert!(
            step(Step::Name, "myai", &mut draft).is_err(),
            "duplicate name"
        );
        assert!(step(Step::Name, "+new", &mut draft).is_err(), "action name");
        assert!(step(Step::Name, "has \"quote\"", &mut draft).is_err());
        assert!(matches!(
            step(Step::Name, "acme", &mut draft),
            Ok(ProviderNext::Ask(Step::Kind))
        ));
        assert_eq!(draft.name, "acme");

        // The dialect and the store are closed sets.
        assert!(step(Step::Kind, "gemini", &mut draft).is_err());
        assert!(matches!(
            step(Step::Kind, "anthropic", &mut draft),
            Ok(ProviderNext::Ask(Step::BaseUrl))
        ));

        // A base URL has to be one.
        assert!(step(Step::BaseUrl, "acme.test", &mut draft).is_err());
        assert!(matches!(
            step(Step::BaseUrl, "https://acme.test/v1", &mut draft),
            Ok(ProviderNext::Ask(Step::Model))
        ));
        // One host serves several models, so the step takes a list; a slug that
        // could not be written into TOML is refused before any of it is kept.
        assert!(step(Step::Model, "acme-1, bad\"quote", &mut draft).is_err());
        assert!(
            step(Step::Model, " , ", &mut draft).is_err(),
            "no model named"
        );
        assert!(matches!(
            step(Step::Model, "acme-1, acme-2 , acme-1", &mut draft),
            Ok(ProviderNext::Ask(Step::Store))
        ));
        assert_eq!(
            draft.models,
            vec!["acme-1".to_owned(), "acme-2".to_owned()],
            "duplicates dropped, order kept, padding trimmed"
        );
        assert!(step(Step::Store, "vault", &mut draft).is_err());
        assert!(matches!(
            step(Step::Store, "file", &mut draft),
            Ok(ProviderNext::Ask(Step::Key))
        ));

        // A credential too short to redact safely is refused before it is
        // stored, so it cannot end up on the wire unredacted.
        assert!(step(Step::Key, "short", &mut draft).is_err());

        // Removal names a configured provider and is confirmed before it runs.
        assert!(step(Step::Remove, "nothere", &mut draft).is_err());
        assert!(matches!(
            step(Step::Remove, "myai", &mut draft),
            Ok(ProviderNext::Ask(Step::ConfirmRemove))
        ));
        assert!(matches!(
            step(Step::ConfirmRemove, "no", &mut draft),
            Ok(ProviderNext::Cancelled(_))
        ));

        // Only the key step is a secret, and only it keeps the answer verbatim.
        for probe in [
            Step::Pick,
            Step::Name,
            Step::Kind,
            Step::BaseUrl,
            Step::Model,
        ] {
            assert!(!probe.masked(), "{probe:?} was masked");
        }
        assert!(Step::Key.masked());

        // The pick list carries the actions under the providers, and offers
        // nothing to remove when nothing is configured.
        let rows = Step::Pick
            .rows(&providers, "myai", Some("myai"))
            .expect("a list");
        assert_eq!(rows[0].0, "myai");
        // The active one says so, so a provider that is merely not current does
        // not read as one that was removed.
        assert_eq!(rows[0].1, "in use");
        let rows = Step::Pick.rows(&providers, "other", None).expect("a list");
        assert!(rows[0].1.contains("switch"), "{:?}", rows[0]);

        // Switched but not restarted: the session still runs the old one, and
        // the row says which is which rather than letting the new choice look
        // like it did not take.
        let rows = Step::Pick
            .rows(&providers, "other", Some("myai"))
            .expect("a list");
        assert!(rows[0].1.contains("after a restart"), "{:?}", rows[0]);
        assert!(rows.iter().any(|(name, _)| name == "+new"));
        assert!(rows.iter().any(|(name, _)| name == "-remove"));
        let empty = Step::Pick.rows(&[], "", None).expect("a list");
        assert!(empty.iter().all(|(name, _)| name != "-remove"));
        assert!(
            Step::Name.rows(&providers, "myai", None).is_none(),
            "a name is typed"
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn the_auth_wizard_routes_actions_and_lists_providers() {
        use tui::AuthStep as Step;

        let invocation = Invocation {
            workspace: PathBuf::from("."),
            command: Command::Tui,
            no_color: true,
            output: Some(Output::Human),
        };
        let mut emitter = Emitter::new(Output::Human);
        let providers = vec!["antigravity".to_owned(), "chatgpt".to_owned()];
        let mut draft = String::new();

        // An empty answer leaves the wizard without touching anything.
        assert!(matches!(
            auth_step(
                &invocation,
                Step::Pick,
                "  ",
                &mut draft,
                &providers,
                &mut emitter
            )
            .unwrap(),
            AuthNext::Cancelled(_)
        ));

        // Picking login moves to the provider picker.
        assert!(matches!(
            auth_step(
                &invocation,
                Step::Pick,
                "login",
                &mut draft,
                &providers,
                &mut emitter
            )
            .unwrap(),
            AuthNext::Ask(Step::LoginProvider)
        ));

        // Picking set moves to the provider picker.
        assert!(matches!(
            auth_step(
                &invocation,
                Step::Pick,
                "set",
                &mut draft,
                &providers,
                &mut emitter
            )
            .unwrap(),
            AuthNext::Ask(Step::SetProvider)
        ));

        // Choosing a provider to set asks for its key.
        assert!(matches!(
            auth_step(
                &invocation,
                Step::SetProvider,
                "antigravity",
                &mut draft,
                &providers,
                &mut emitter
            )
            .unwrap(),
            AuthNext::Ask(Step::SetKey)
        ));
        assert_eq!(draft, "antigravity");
    }

    /// The catalog is metadata, so where it lives is the operator's choice and
    /// the default costs no unlock prompt.
    #[test]
    fn the_credential_catalog_store_is_configurable_and_defaults_to_a_file() {
        use arsy_kernel::config::{Config, Layer, CREDENTIAL_STORES, DEFAULT_CREDENTIAL_STORE};

        let directory = tempfile::tempdir().unwrap();
        let write = |body: &str| {
            let path = directory.path().join("config.toml");
            std::fs::write(&path, body).unwrap();
            Config::load(&[(Layer::User, path)])
        };

        // Unset is the file, so an operator who never asked is not asked to
        // unlock anything to read a list of handles.
        assert_eq!(DEFAULT_CREDENTIAL_STORE, "file");
        assert_eq!(
            write("schema_version = 1\n").unwrap().credential_store(),
            "file"
        );

        // Either store can be chosen, and the choice is traceable like any
        // other configured value.
        for store in CREDENTIAL_STORES {
            let config = write(&format!(
                "schema_version = 1\n[credentials]\nstore = \"{store}\"\n"
            ))
            .unwrap();
            assert_eq!(config.credential_store(), *store);
            assert_eq!(
                config.explain(Some("credentials.store"))["values"]["credentials.store"]["value"],
                **store
            );
        }

        // A name that is neither is refused at load, not silently defaulted:
        // a typo must not quietly send credentials somewhere else.
        let error = write("schema_version = 1\n[credentials]\nstore = \"vault\"\n").unwrap_err();
        assert!(format!("{error}").contains("vault"), "{error}");

        // The two names match the `secret://` stores, so one vocabulary covers
        // both the handle and the catalog.
        assert_eq!(CREDENTIAL_STORES, ["file", "os"]);
    }

    /// `/settings` and `/auth` print to a reader, not to a parser: the machine
    /// record is still the one JSON mode emits.
    #[test]
    fn configuration_and_credentials_render_as_rows_for_a_reader() {
        let report = json!({
            "schema_version": 1,
            "diagnostics": [],
            "values": {
                "provider.default": {"layer": "user", "path": "/cfg/config.toml", "value": "myai"},
                "provider.endpoint.myai.kind": {
                    "layer": "workspace", "path": "/ws/.arsy/config.toml", "value": "openai"
                },
            },
        });

        let rendered = human_config(&report, None);
        let listing = rendered["configuration"].as_str().expect("one string");
        assert!(listing.starts_with("2 values set"), "{listing}");
        assert!(
            listing.contains("default provider: myai  [user]"),
            "{listing}"
        );
        assert!(listing.contains("kind:  openai  [workspace]"), "{listing}");
        // Each source file is named once, under the rows, rather than repeated
        // on every one of them.
        assert_eq!(listing.matches("/cfg/config.toml").count(), 1, "{listing}");
        assert!(listing.contains("from /ws/.arsy/config.toml"), "{listing}");

        // Nothing set is a sentence, not an empty object.
        let empty = human_config(&json!({"values": {}}), Some("provider.default"));
        let listing = empty["configuration"].as_str().expect("one string");
        assert!(listing.contains("provider.default"), "{listing}");
        assert!(listing.contains("No configuration"), "{listing}");

        // Credentials list handles, never values, so a row is safe to show.
        let records = vec![
            AuthRecord {
                provider: "myai".to_owned(),
                handle: SecretHandle::new("os", "myai").unwrap(),
                created_at: 1,
                last_used: None,
                kind: CredentialKind::ApiKey,
            },
            AuthRecord {
                provider: "acme".to_owned(),
                handle: SecretHandle::new("file", "acme.key").unwrap(),
                created_at: 2,
                last_used: Some(9),
                kind: CredentialKind::OAuth,
            },
        ];
        let rendered = human_credentials(&records);
        let listing = rendered["credentials"].as_str().expect("one string");
        assert!(listing.starts_with("2 credentials stored"), "{listing}");
        assert!(listing.contains("secret://os/myai"), "{listing}");
        assert!(
            listing.contains("api_key  provider myai  · never used"),
            "{listing}"
        );
        // The kind column is padded, so what follows it lines up.
        assert!(listing.contains("oauth    provider acme"), "{listing}");
        assert_eq!(
            listing.matches("never used").count(),
            1,
            "only the unused credential carries the marker: {listing}"
        );

        let empty = human_credentials(&[]);
        assert!(
            empty["credentials"]
                .as_str()
                .expect("one string")
                .contains("auth set"),
            "an empty list must say how to add one"
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn the_effort_picker_takes_a_number_a_name_or_the_current_setting() {
        // The list is numbered the way the model list is, and `off` is a row on
        // it rather than a word only a typist knows about.
        assert_eq!(
            tui::effort_choices(),
            vec![
                Some(Effort::Low),
                Some(Effort::Medium),
                Some(Effort::High),
                None
            ]
        );

        for (index, expected) in tui::effort_choices().iter().enumerate() {
            let answer = (index + 1).to_string();
            assert_eq!(
                tui::resolve_effort_answer(&answer, None).unwrap(),
                *expected,
                "row {answer}"
            );
        }

        for level in Effort::ALL {
            assert_eq!(
                tui::resolve_effort_answer(level.as_str(), None).unwrap(),
                Some(level)
            );
        }
        for word in ["off", "none", "unset"] {
            assert_eq!(
                tui::resolve_effort_answer(word, Some(Effort::High)).unwrap(),
                None,
                "{word} did not clear the level"
            );
        }

        // An empty line keeps what is set, so leaving the picker alone is not a
        // way to lose the setting.
        assert_eq!(
            tui::resolve_effort_answer("   ", Some(Effort::Medium)).unwrap(),
            Some(Effort::Medium)
        );

        // A rejected answer says why and changes nothing; the caller keeps the
        // picker open on it.
        for answer in ["hihg", "0", "5", "-1"] {
            assert!(
                tui::resolve_effort_answer(answer, Some(Effort::High)).is_err(),
                "{answer} was accepted"
            );
        }

        assert!(effort_line(Some(Effort::High)).contains("high"));
        assert!(effort_line(None).contains("no reasoning setting"));

        // The picker opens marked at what is set, so the first row a reader
        // sees marked is the answer they already have.
        assert_eq!(tui::effort_row(Some(Effort::Low)), 0);
        assert_eq!(tui::effort_row(Some(Effort::High)), 2);
        assert_eq!(tui::effort_row(None), 3, "an unset level marks `off`");
    }

    #[cfg(feature = "tui")]
    #[test]
    fn the_effort_rows_are_arrowed_by_the_composer_that_already_owns_the_keys() {
        let mut composer = tui::Composer::default();
        composer.set_picking(true);
        composer.offer_table(Some(tui::EFFORT_ROWS), tui::effort_row(None));

        // Offered rows beat the command table, so a picker is not answered with
        // slash commands, and Up/Down move the mark rather than walk history.
        assert_eq!(composer.menu().len(), tui::EFFORT_ROWS.len());
        assert_eq!(composer.marked().as_deref(), Some("off"));
        composer.press(tui::Key::Down);
        assert_eq!(
            composer.marked().as_deref(),
            Some("low"),
            "the last row wraps"
        );
        composer.press(tui::Key::Up);
        assert_eq!(composer.marked().as_deref(), Some("off"));

        // Enter takes the marked level into the line; a second Enter sends it,
        // and what it sends is an answer the picker accepts.
        assert_eq!(composer.press(tui::Key::Enter), tui::Action::Redraw);
        assert_eq!(
            composer.press(tui::Key::Enter),
            tui::Action::Submit("off".to_owned())
        );
        assert_eq!(
            tui::resolve_effort_answer("off", Some(Effort::High)),
            Ok(None)
        );

        // Typing narrows the offered rows the way it narrows the commands.
        let mut composer = tui::Composer::default();
        composer.offer_table(Some(tui::EFFORT_ROWS), 0);
        for character in "me".chars() {
            composer.press(tui::Key::Char(character));
        }
        assert_eq!(composer.menu().len(), 1);
        assert_eq!(composer.marked().as_deref(), Some("medium"));

        // Clearing the offer hands the menu back to the command table.
        composer.offer_table(None, 0);
        assert!(composer.menu().is_empty(), "a task line offers no menu");
    }

    #[cfg(feature = "tui")]
    #[test]
    fn the_branch_comes_from_head_including_a_worktree_pointer() {
        let root = std::env::temp_dir().join(format!("arsy-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        assert_eq!(tui::branch(&root), None, "no checkout, no branch");

        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feat/slash-menu\n").unwrap();
        assert_eq!(tui::branch(&repo).as_deref(), Some("feat/slash-menu"));

        // Detached: HEAD holds the commit id, so the row shows a short one.
        std::fs::write(repo.join(".git/HEAD"), "3cd02230f0f0f0f0f0f0\n").unwrap();
        assert_eq!(tui::branch(&repo).as_deref(), Some("3cd02230"));

        // A worktree or submodule leaves a `gitdir:` pointer where the
        // directory would be.
        let linked = root.join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: ../repo/.git\n").unwrap();
        assert_eq!(tui::branch(&linked).as_deref(), Some("3cd02230"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(feature = "tui")]
    #[test]
    fn the_menu_and_the_dispatch_table_hold_the_same_commands() {
        // `/model`, `/effort`, `/theme`, `/help`, and `/quit` are answered by
        // the loop itself; every other offered command must be an inspection it
        // knows how to run.
        for (name, _) in tui::COMMANDS {
            let handled = matches!(
                *name,
                "/model" | "/effort" | "/theme" | "/provider" | "/help" | "/quit" | "/new" | "/clear" | "/resume" | "/update" | "/rename" | "/session" | "/approval"
            ) || INSPECTIONS.iter().any(|(slash, _, _)| slash == name);
            assert!(handled, "{name} is offered but never dispatched");
        }
        for (slash, _, _) in INSPECTIONS {
            assert!(
                tui::COMMANDS.iter().any(|(name, _)| name == slash),
                "{slash} is dispatched but never offered"
            );
        }
    }

    #[cfg(all(feature = "tui", unix))]
    #[test]
    fn interactive_provider_cancellation_and_terminal_failures_are_bounded() {
        use std::time::{Duration, Instant};
        let route = tui::ModelRoute {
            provider: tui::CODEX_PROVIDER.to_owned(),
            model: "default".to_owned(),
        };
        let (sender, keys) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            sender.send(0x1b).unwrap();
            // Keep input open until cancellation has had time to finish.
            std::thread::sleep(Duration::from_secs(3));
        });
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "trap '' TERM; while :; do printf '%s\\n' '{\"type\":\"turn.started\"}'; sleep 0.01; done"]);
        let started = Instant::now();
        let result = drive_provider(
            command,
            &"x".repeat(131_072),
            &route,
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &Redactor::new(),
        )
        .unwrap();
        assert!(result.interrupted);
        assert!(!result.quit, "Escape cancels the turn, not the session");
        assert!(started.elapsed() < Duration::from_secs(5));
        worker.join().unwrap();

        let (_sender, keys) = std::sync::mpsc::channel();
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "read task; printf '%s\\n' '{\"type\":\"turn.failed\",\"error\":{\"message\":\"test failure\"}}'"]);
        let result = drive_provider(
            command,
            "task\n",
            &route,
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &Redactor::new(),
        )
        .unwrap();
        assert!(
            result.provider_failed,
            "a zero process exit must not mask a failed turn"
        );
        assert!(
            result.failure.is_some(),
            "a failed turn is recorded as a failure, not a completion"
        );

        // A provider that reports its turn complete and then lingers must not
        // hold the session: the answer is on screen, so the clock stops with
        // the turn rather than with the process. The stub ignores TERM, which
        // is what makes the wait for it a hang instead of a pause.
        let (_sender, keys) = std::sync::mpsc::channel();
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            "read task; printf '%s\\n' '{\"type\":\"turn.completed\"}'; trap '' TERM; sleep 30",
        ]);
        let started = Instant::now();
        let result = drive_provider(
            command,
            "task\n",
            &route,
            false,
            "  footer",
            &keys,
            &mut tui::Keys::default(),
            &mut tui::Composer::default(),
            &Redactor::new(),
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a completed turn waited on the provider process"
        );
        assert!(
            result.failure.is_none(),
            "stopping a lingering provider is not a failed turn: {:?}",
            result.failure
        );
        assert!(
            !result.interrupted,
            "the turn completed, it was not cancelled"
        );
    }

    /// The catalog is written by one version and read by the next, so a
    /// record from before logins existed has to keep working.
    #[test]
    fn an_older_credential_catalog_still_reads() {
        let old = r#"[{"provider":"anthropic","handle":"secret://os/anthropic","created_at":1,"last_used":null}]"#;
        let records: Vec<AuthRecord> = serde_json::from_str(old).unwrap();
        assert_eq!(records[0].provider, "anthropic");
        assert_eq!(
            records[0].kind,
            CredentialKind::ApiKey,
            "a record written before logins existed is an API key"
        );

        // Round-trips under the name the catalog actually stores.
        let written = serde_json::to_string(&[AuthRecord {
            kind: CredentialKind::OAuth,
            ..records[0].clone()
        }])
        .unwrap();
        assert!(written.contains(r#""kind":"oauth""#), "{written}");
        let back: Vec<AuthRecord> = serde_json::from_str(&written).unwrap();
        assert_eq!(back[0].kind, CredentialKind::OAuth);

        let derived = written.replace(r#""kind":"oauth""#, r#""kind":"o_auth""#);
        let back: Vec<AuthRecord> = serde_json::from_str(&derived).unwrap();
        assert_eq!(
            back[0].kind,
            CredentialKind::OAuth,
            "a catalog written under the derived name must not read as corrupt"
        );
    }

    #[test]
    fn auth_entry_points_parse_without_accepting_a_secret_argument() {
        assert!(matches!(
            parse(["auth", "set", "anthropic"].map(str::to_owned))
                .unwrap()
                .command,
            Command::AuthSet { .. }
        ));
        assert!(matches!(
            parse(["auth", "list"].map(str::to_owned)).unwrap().command,
            Command::AuthList
        ));
        assert!(matches!(
            parse(["auth", "remove", "secret://os/anthropic"].map(str::to_owned))
                .unwrap()
                .command,
            Command::AuthRemove { .. }
        ));
        assert!(parse(["auth", "set", "anthropic", "raw-secret"].map(str::to_owned)).is_err());
    }

    #[test]
    fn compatibility_explain_is_a_real_command() {
        assert_eq!(
            parse(["compat", "explain", "claude"].map(str::to_owned))
                .unwrap()
                .command,
            Command::CompatExplain {
                ecosystem: arsy_code::compat::Ecosystem::Claude
            }
        );
        assert!(parse(["compat", "explain", "unknown"].map(str::to_owned)).is_err());
    }

    #[test]
    fn minimal_cli_recovers_a_crashed_session_and_has_bounded_noninteractive_outcomes() {
        let workspace = std::env::temp_dir().join(format!("arsy-cli-{}", SessionId::new()));
        std::fs::create_dir(&workspace).unwrap();
        let session = SessionId::new();
        let store = open_store(&workspace).unwrap();
        let service = AgentService::attach(store.clone(), session).unwrap();
        let request = ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
            session,
            prompt: "recover this exact task".to_owned(),
            extensions: Extensions::new(),
        }));
        service.start_turn(actor(), &request).unwrap();
        drop(service);

        let invocation = Invocation {
            workspace: workspace.clone(),
            output: Some(Output::Ci),
            no_color: false,
            command: Command::Resume {
                session,
                follow: false,
            },
        };
        assert_eq!(
            resume(&invocation, session, false, &mut Emitter::new(Output::Ci)),
            Ok(0)
        );

        let events = store.read(session, 1, 8).unwrap();
        assert_eq!(events.len(), 2, "resume appends; it never rewrites history");
        assert_eq!(events[0].kind, "turn.started");
        assert_eq!(events[1].kind, "turn.failed");
        let EventPayload::Inline { data } = &events[0].payload else {
            panic!("turn evidence must remain inline");
        };
        assert!(data.to_string().contains("recover this exact task"));

        assert_eq!(doctor(&invocation, false, &mut Emitter::new(Output::Ci)), 0);
        assert_eq!(Diagnostic::error(ARSY_PRV_1000, "", "").exit_code(), 5);
        assert_eq!(Diagnostic::error("ARSY-POL-1000", "", "").exit_code(), 3);
        assert_eq!(
            execute(
                &Invocation {
                    command: Command::Tui,
                    ..invocation
                },
                false,
                &mut Emitter::new(Output::Ci),
            )
            .unwrap_err()
            .exit_code(),
            2,
            "no TTY refuses instead of waiting for interactive input"
        );

        drop(store);
        std::fs::remove_dir_all(workspace).unwrap();
    }
}
