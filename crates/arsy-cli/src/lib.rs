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
//! | `ARSY-PRV-1000` | no provider credential is available, so the turn cannot dispatch |
//! | `ARSY-PRV-1002` | an installed provider CLI failed |
//! | `ARSY-SBX-1000` | no sandbox worker is available on this build |
//! | `ARSY-PRV-1001` | no credential store is registered |
//! | `ARSY-UIX-1000` | interactive terminal input or output failed |

mod eval;
mod integrations;
#[cfg(feature = "tui")]
pub mod tui;

use arsy_kernel::{
    domain::{Principal, SessionId},
    event::EventStore,
    protocol::{ClientRequest, Extensions, ProtocolEnvelope, TurnStart},
    secret::{
        CredentialStore, OsCredentialStore, Redactor, SecretBroker, SecretError, SecretHandle,
        OS_STORE_ID,
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
    time::{SystemTime, UNIX_EPOCH},
};

/// Every session of one workspace shares this store.
const STORE_PATH: &str = ".arsy/sessions.sqlite3";
/// Machine records carry the protocol's schema version.
const RECORD_SCHEMA: u32 = 1;

/// Documented commands that a later phase ships, so they report their phase
/// instead of failing as unknown input.
const UNAVAILABLE: &[(&str, u8)] = &[
    ("artifact", 1),
    ("completions", 1),
    ("config", 1),
    ("gc", 1),
    ("migrate", 1),
    ("model", 2),
    ("plugin", 8),
    ("policy", 1),
    ("provider", 1),
    ("review", 6),
    ("serve", 1),
    ("session", 1),
    ("skill", 5),
];

const USAGE: &str = "\
arsy — agentic coding harness

Usage:
  arsy run <TASK>            execute one task non-interactively ('-' reads stdin)
  arsy resume <SESSION_ID>   resume a recorded session
  arsy doctor                report platform, sandbox, credential, and config state
  arsy eval <SUITE>          run a pinned evaluation fixture
  arsy compat explain <KIND> explain claude, codex, omp, or agents imports
  arsy mcp list [--source <KIND>]       inspect imported MCP declarations
  arsy mcp show <NAME> [--source <KIND>] show one MCP declaration
  arsy hook list [--event <NAME>]      inspect imported lifecycle hooks
  arsy auth set <PROVIDER>   store a credential in the OS credential store
  arsy auth list             list credential handles (never values)
  arsy auth remove <HANDLE>  remove a credential from the OS credential store

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
    AuthSet {
        provider: String,
        handle: Option<String>,
    },
    AuthList,
    AuthRemove {
        handle: SecretHandle,
        force: bool,
    },
    Eval {
        suite: PathBuf,
        trials: Option<u32>,
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
        && !matches!(parsed.name.as_deref(), Some("mcp" | "hook"))
    {
        return Err(usage(
            "--source and --event apply only to MCP/hook inspection",
        ));
    }
    let command = match parsed.name.as_deref() {
        None => Command::Tui,
        Some("run") => Command::Run {
            task: only_argument(parsed.positional, "run", "<TASK>")?,
        },
        Some("resume") => parse_resume(parsed.positional, parsed.follow)?,
        Some("doctor") => parse_doctor(parsed.positional, parsed.strict)?,
        Some("eval") => Command::Eval {
            suite: PathBuf::from(only_argument(parsed.positional, "eval", "<SUITE>")?),
            trials: parsed.trials,
            out: parsed.out,
        },
        Some("compat") => Command::CompatExplain {
            ecosystem: compatibility_kind(parsed.positional)?,
        },
        Some("auth") => parse_auth(parsed.positional, parsed.handle, parsed.force)?,
        Some("mcp" | "hook") => integrations::parse(
            parsed.name.as_deref().unwrap(),
            parsed.positional,
            parsed.source,
            parsed.event,
        )?,
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
    follow: bool,
    strict: bool,
    force: bool,
    handle: Option<String>,
    trials: Option<u32>,
    out: Option<PathBuf>,
    source: Option<String>,
    event: Option<String>,
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
        "--follow" => parsed.follow = true,
        "--strict" => parsed.strict = true,
        "--force" => parsed.force = true,
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
        "--out" => parsed.out = Some(PathBuf::from(value(arguments, argument)?)),
        "--source" => parsed.source = Some(value(arguments, argument)?),
        "--event" => parsed.event = Some(value(arguments, argument)?),
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
        Some("list") if positional.len() == 1 => Ok(Command::AuthList),
        Some("remove") => {
            positional.remove(0);
            let raw = only_argument(positional, "auth remove", "<HANDLE>")?;
            Ok(Command::AuthRemove {
                handle: raw.try_into().map_err(secret_failed)?,
                force,
            })
        }
        _ => Err(usage("auth requires set, list, or remove")),
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
}

impl Emitter {
    const fn new(output: Output) -> Self {
        Self {
            output,
            session: None,
            sequence: 0,
            redactor: Redactor::new(),
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
        Command::AuthSet { provider, handle } => {
            auth_set(provider, handle.as_deref(), tty, emitter)
        }
        Command::AuthList => auth_list(emitter),
        Command::AuthRemove { handle, force } => auth_remove(handle, *force, emitter),
        Command::Eval { suite, trials, out } => {
            let workspace = workspace_root(&invocation.workspace)?;
            let report = eval::run(&workspace, suite, *trials, out.as_deref())?;
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
    }
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

const CATALOG_NAME: &str = "__catalog__";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AuthRecord {
    provider: String,
    handle: SecretHandle,
    created_at: u64,
    last_used: Option<u64>,
}

fn catalog(store: OsCredentialStore) -> Result<Vec<AuthRecord>, Diagnostic> {
    match store.resolve(CATALOG_NAME) {
        Ok(raw) => {
            serde_json::from_str(&raw).map_err(|_| secret_failed("credential catalog is corrupt"))
        }
        Err(SecretError::NotFound(_)) => Ok(Vec::new()),
        Err(error) => Err(secret_failed(error)),
    }
}

fn save_catalog(store: OsCredentialStore, records: &[AuthRecord]) -> Result<(), Diagnostic> {
    let raw = serde_json::to_string(records).map_err(|error| secret_failed(error.to_string()))?;
    store.set(CATALOG_NAME, &raw).map_err(secret_failed)
}

fn auth_set(
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
    let store = OsCredentialStore;
    let previous = match store.resolve(name) {
        Ok(value) => Some(value),
        Err(SecretError::NotFound(_)) => None,
        Err(error) => return Err(secret_failed(error)),
    };
    store.set(name, &secret).map_err(secret_failed)?;
    let mut records = catalog(store)?;
    let now = now()?;
    if let Some(record) = records.iter_mut().find(|record| record.handle == handle) {
        record.provider = provider.to_owned();
    } else {
        records.push(AuthRecord {
            provider: provider.to_owned(),
            handle: handle.clone(),
            created_at: now,
            last_used: None,
        });
    }
    if let Err(error) = save_catalog(store, &records) {
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

fn auth_list(emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let records = catalog(OsCredentialStore)?;
    emitter.result(json!({"credentials": records}));
    Ok(0)
}

fn auth_remove(
    handle: &SecretHandle,
    _force: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    if handle.store() != OS_STORE_ID {
        return Err(secret_failed("only OS credential handles can be removed"));
    }
    let store = OsCredentialStore;
    let original = catalog(store)?;
    let mut records = original.clone();
    records.retain(|record| &record.handle != handle);
    save_catalog(store, &records)?;
    if let Err(error) = store.remove(handle.name()) {
        let _ = save_catalog(store, &original);
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
}

#[cfg(feature = "tui")]
fn run_tui(invocation: &Invocation, emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let workspace = workspace_root(&invocation.workspace)?;
    let mut stdout = io::stdout();
    let detected = tui::detect_model_route();
    let provider_available = detected.is_some();
    let detected = detected.unwrap_or_else(|| tui::ModelRoute {
        model: "default".into(),
    });
    let colour = !invocation.no_color && std::env::var_os("NO_COLOR").is_none();
    let models = tui::available_models();
    let mut route = saved_model().map_or(detected, |model| tui::ModelRoute { model });

    let mut state = tui::TuiState::new(workspace.display().to_string(), SessionId::new());
    state.set_model_route(route.clone());
    writeln!(stdout, "{}", state.render(tui::terminal_width(), colour)).map_err(terminal_failed)?;
    writeln!(
        stdout,
        "Use /help for commands, /mcp and /hooks to inspect integrations."
    )
    .map_err(terminal_failed)?;
    if !provider_available {
        writeln!(stdout, "Provider unavailable. Inspection is available; install Codex and run codex login to execute tasks.").map_err(terminal_failed)?;
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
    let mut prompt = if saved_model().is_some() || !provider_available {
        Prompt::Task
    } else {
        tui::render_model_list(&mut stdout, &models, &route, colour).map_err(terminal_failed)?;
        Prompt::Model
    };

    loop {
        let status = match prompt {
            Prompt::Task => state.status_row(tui::terminal_width(), colour),
            Prompt::Model => tui::model_prompt(&models, &route, colour),
        };
        // Derived from the prompt once per line, so the command menu can never
        // drift out of step with which prompt is collecting the answer.
        composer.set_picking(matches!(prompt, Prompt::Model));
        let line = match queued.pop_front() {
            Some(line) => line,
            None => match read_line(
                &keys,
                &mut decoder,
                &mut composer,
                &mut stdout,
                colour,
                &status,
            )? {
                Some(line) => line,
                // Ending input at the picker cancels the picker, not the
                // session: the model is unchanged and the task prompt returns.
                None if matches!(prompt, Prompt::Model) => {
                    write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                    writeln!(stdout, "Model unchanged: {route}").map_err(terminal_failed)?;
                    prompt = Prompt::Task;
                    continue;
                }
                None => break,
            },
        };
        match prompt {
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
            Prompt::Task if line.trim() == "/model" => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                tui::render_model_list(&mut stdout, &models, &route, colour)
                    .map_err(terminal_failed)?;
                prompt = Prompt::Model;
            }
            Prompt::Task if matches!(line.trim(), ":quit" | "/quit" | "/exit") => break,
            Prompt::Task if line.trim().starts_with('/') => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                let mut words = line.split_whitespace();
                let command = words.next().unwrap_or_default();
                if command == "/help" {
                    write!(stdout, "{}", tui::help(colour)).map_err(terminal_failed)?;
                } else if matches!(command, "/mcp" | "/hooks") {
                    let kind = if command == "/mcp" { "mcp" } else { "hook" };
                    let mut args = vec![kind.to_owned()];
                    args.extend(words.map(str::to_owned));
                    if args.get(1).is_none_or(|arg| arg.starts_with("--")) {
                        args.insert(1, "list".into());
                    }
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
                        "ARSY-PRV-1000",
                        "provider unavailable",
                        "run codex login and restart ARSY; /mcp and /hooks remain available",
                    ));
                    continue;
                }
                match run_external(
                    invocation,
                    &line,
                    &route,
                    colour,
                    &keys,
                    &mut decoder,
                    &mut composer,
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
    if let Err(error) = save_model(&route.model) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-UIX-1001",
            format!("the model choice was not remembered: {error}"),
            "check that the ARSY user configuration directory is writable",
        ));
    }
}

/// The remembered model lives beside the user configuration layer that
/// `arsy doctor` already reports.
#[cfg(feature = "tui")]
fn model_store() -> Option<PathBuf> {
    Some(user_config()?.with_file_name("model"))
}

/// A remembered model is re-validated on read: a file written by an older build
/// that accepted anything must not keep selecting an unusable model on every
/// later start.
#[cfg(feature = "tui")]
fn saved_model() -> Option<String> {
    let model = std::fs::read_to_string(model_store()?).ok()?;
    let model = model.trim();
    tui::validate_slug(model).ok()?;
    Some(model.to_owned())
}

#[cfg(feature = "tui")]
fn save_model(model: &str) -> io::Result<()> {
    let path = model_store()
        .ok_or_else(|| io::Error::other("this platform has no user configuration directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{model}\n"))
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn run_external(
    invocation: &Invocation,
    task: &str,
    route: &tui::ModelRoute,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    emitter: &mut Emitter,
) -> Result<Turn, Diagnostic> {
    let task = prepare_task(task, emitter)?;
    let (service, actor, admission, session) = record_turn(invocation, task.clone(), emitter)?;
    let outcome = external_status(
        &workspace_root(&invocation.workspace)?,
        &task,
        route,
        colour,
        keys,
        decoder,
        composer,
        &emitter.redactor,
    );
    if outcome.is_err() {
        let mut stdout = io::stdout();
        write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
        stdout.flush().map_err(terminal_failed)?;
    }
    let (turn, status) = match outcome {
        Ok(turn) => {
            let status = turn.status.ok_or(turn.interrupted);
            (turn, status)
        }
        Err(error) => {
            fail_external_turn(
                &service,
                actor,
                admission.turn,
                session,
                format!("could not run {route}: {error}"),
                emitter,
            )?;
            return Ok(Turn::default());
        }
    };
    if turn.interrupted {
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
    match status {
        Ok(status) if status.success() && !turn.provider_failed => {
            let outcome = json!({"provider": "codex", "model": route.model});
            service
                .complete_turn(actor, admission.turn, &outcome)
                .map_err(storage_failed)?;
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
        Ok(status) => {
            fail_external_turn(
                &service,
                actor,
                admission.turn,
                session,
                if turn.provider_failed {
                    format!("{route} reported a failed turn")
                } else {
                    format!("{route} exited with status {status}")
                },
                emitter,
            )?;
        }
        Err(_) => {
            fail_external_turn(
                &service,
                actor,
                admission.turn,
                session,
                format!("{route} produced no exit status"),
                emitter,
            )?;
        }
    }
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
    drive_provider(command, task, colour, keys, decoder, composer, redactor)
}

#[cfg(feature = "tui")]
fn drive_provider(
    mut command: std::process::Command,
    task: &str,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
    redactor: &Redactor,
) -> io::Result<Turn> {
    #[cfg(unix)]
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
    let mut finished = false;
    let mut last_row = None;
    let mut last_key = std::time::Instant::now();
    let mut exited = None;
    let mut outcome = Turn::default();
    let mut terminal = io::stdout();
    let width = std::cell::Cell::new(tui::terminal_width());
    composer.set_height(tui::terminal_rows());
    let draw = |terminal: &mut io::Stdout,
                composer: &mut tui::Composer,
                row: Option<&str>,
                cancelling: bool,
                queued: usize| {
        // Rows land above the composer, which is torn down and repainted around
        // each one so the input block is never overwritten.
        let mut frame = composer.clear();
        if let Some(row) = row {
            frame.push_str(row);
            frame.push('\n');
        }
        // The queue is only mentioned once there is one: a permanent `0 queued`
        // is noise on the line the terminal shows for the whole turn.
        let status = format!(
            "{} · {}s{} · Esc cancel",
            if cancelling {
                "  Cancelling…".to_owned()
            } else {
                tui::working_row(colour)
            },
            started.elapsed().as_secs(),
            if queued == 0 {
                String::new()
            } else {
                format!(" · {queued} queued")
            }
        );
        frame.push_str(&composer.render(width.get(), colour, &status));
        write!(terminal, "{frame}").and_then(|()| terminal.flush())
    };
    draw(&mut terminal, composer, None, false, 0)?;
    let mut refreshed = std::time::Instant::now();
    loop {
        if let Ok(result) = input.try_recv() {
            if !outcome.interrupted {
                result?;
            }
        }
        if outcome.status.is_none() {
            outcome.status = child.0.try_wait()?;
            if outcome.status.is_some() {
                exited = Some(std::time::Instant::now());
                child.stop(true);
            }
        }
        if exited
            .is_some_and(|at: std::time::Instant| at.elapsed() >= std::time::Duration::from_secs(2))
        {
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
            outcome.status = Some(child.0.wait()?);
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
                    )?;
                }
                continue;
            }
            match composer.press(key) {
                // A line sent while the provider is busy runs as soon as this
                // turn ends, rather than being dropped or blocking.
                tui::Action::Submit(line) if !line.trim().is_empty() => {
                    if outcome.queued.len() < 16 {
                        outcome.queued.push_back(line);
                        draw(
                            &mut terminal,
                            composer,
                            Some("  Follow-up queued."),
                            cancelling.is_some(),
                            outcome.queued.len(),
                        )?;
                    } else {
                        composer.restore(line);
                        draw(
                            &mut terminal,
                            composer,
                            Some("  Queue full; draft retained."),
                            cancelling.is_some(),
                            outcome.queued.len(),
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
            )?;
        }
        match events.recv_timeout(std::time::Duration::from_millis(40)) {
            Ok(_) if outcome.interrupted => {}
            Ok(line) => {
                let line = line?;
                let line = redactor.sanitize(&line).map_err(io::Error::other)?;
                let event = serde_json::from_str::<Value>(&line)
                    .map_err(|_| io::Error::other("provider emitted invalid JSON"))?;
                if matches!(
                    event["type"].as_str(),
                    Some("turn.completed" | "turn.failed")
                ) {
                    finished = true;
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
                            )?;
                        }
                        last_row = Some(row);
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => stream_closed = true,
        }
        if stream_closed {
            if outcome.status.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    }
    write!(terminal, "{}", composer.clear())?;
    terminal.flush()?;
    if !outcome.interrupted && !finished {
        let detail = error_output
            .recv_timeout(std::time::Duration::from_millis(100))
            .unwrap_or_default();
        let detail = redactor.sanitize(&detail).map_err(io::Error::other)?;
        return Err(io::Error::other(format!(
            "provider closed its stream without a terminal turn event: {}",
            terminal_text(detail.trim())
        )));
    }
    Ok(outcome)
}

/// What one interactive turn left behind.
#[cfg(feature = "tui")]
#[derive(Default)]
struct Turn {
    status: Option<std::process::ExitStatus>,
    interrupted: bool,
    provider_failed: bool,
    /// A line submitted while this turn was still running.
    queued: std::collections::VecDeque<String>,
    quit: bool,
}

#[cfg(feature = "tui")]
fn fail_external_turn(
    service: &AgentService,
    actor: Principal,
    turn: arsy_kernel::domain::TurnId,
    session: SessionId,
    message: String,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let diagnostic = Diagnostic::error(
        "ARSY-PRV-1002",
        message,
        "verify the selected CLI login and model, then retry",
    );
    service
        .fail_turn(actor, turn, "provider_cli", diagnostic.message.clone())
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
    let task = prepare_task(&task, emitter)?;
    let (service, actor, admission, session) = record_turn(invocation, task, emitter)?;

    // ponytail: the turn is durable before dispatch, so this failure is
    // recoverable by `arsy resume`. Direct provider transport is not wired yet;
    // the interactive TUI can use a logged-in Codex CLI without copying its token.
    let diagnostic = Diagnostic::error(
        "ARSY-PRV-1000",
        "no provider credential is available, so the turn was not dispatched",
        "use the interactive TUI with a logged-in Codex CLI",
    );
    service
        .fail_turn(
            actor,
            admission.turn,
            "provider_auth",
            diagnostic.message.clone(),
        )
        .map_err(storage_failed)?;
    emitter.diagnostic(&diagnostic);
    emitter.result(json!({
        "session": session.to_string(),
        "turn": admission.turn.to_string(),
        "status": "failed",
    }));
    Ok(diagnostic.exit_code())
}

fn prepare_task(task: &str, emitter: &mut Emitter) -> Result<String, Diagnostic> {
    if task.trim().is_empty() {
        return Err(usage("run requires a non-empty task"));
    }
    let mut broker = SecretBroker::new();
    broker.register_store(Box::new(OsCredentialStore));
    for record in catalog(OsCredentialStore)? {
        broker.resolve(&record.handle).map_err(secret_failed)?;
    }
    let task = broker.redactor().sanitize(task).map_err(secret_failed)?;
    emitter.install_redactor(broker.redactor().clone());
    Ok(task)
}

fn record_turn(
    invocation: &Invocation,
    task: String,
    emitter: &mut Emitter,
) -> Result<
    (
        AgentService,
        Principal,
        arsy_kernel::service::TurnAdmission,
        SessionId,
    ),
    Diagnostic,
> {
    let store = open_store(&workspace_root(&invocation.workspace)?)?;
    let session = SessionId::new();
    emitter.session = Some(session);
    let service = AgentService::attach(store, session).map_err(storage_failed)?;
    let actor = actor();
    let envelope = ProtocolEnvelope::new(ClientRequest::TurnStart(TurnStart {
        session,
        prompt: task,
        extensions: Extensions::new(),
    }));
    let admission = service
        .start_turn(actor.clone(), &envelope)
        .map_err(storage_failed)?;
    Ok((service, actor, admission, session))
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
    let service = AgentService::attach(store, session).map_err(storage_failed)?;
    let actor = actor();
    // History is never truncated: a turn that was running when the process died
    // is closed by appending `turn.failed` after the events it already wrote.
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
    // ponytail: following live events needs the serve loop that phase 2 adds;
    // the flag is accepted and reports the committed head instead of hanging.
    emitter.result(json!({
        "session": session.to_string(),
        "events": version.0,
        "closed_turns": interrupted.len(),
        "following": follow,
    }));
    Ok(0)
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
    let config: Vec<Value> = config_layers(workspace.as_deref().unwrap_or(Path::new(".")))
        .into_iter()
        .map(|(layer, path)| {
            json!({
                "layer": layer,
                "path": path.display().to_string(),
                "present": path.is_file(),
            })
        })
        .collect();

    let sandbox_assurance = installed_sandbox_assurance();
    if sandbox_assurance == arsy_kernel::policy::SandboxAssurance::None {
        warnings.push(Diagnostic::warning(
            "ARSY-SBX-1000",
            "no complete sandbox worker is available, so achieved assurance is `none`",
            "install arsy-sandbox-worker and the platform controls before running effects",
        ));
    }
    let credentials = catalog(OsCredentialStore).unwrap_or_default().len();
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

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Configuration layers in authority order, per `docs/35-configuration.md`.
fn config_layers(workspace: &Path) -> Vec<(&'static str, PathBuf)> {
    let mut layers = Vec::new();
    if let Some(path) = enterprise_config() {
        layers.push(("enterprise", path));
    }
    if let Some(path) = user_config() {
        layers.push(("user", path));
    }
    layers.push(("workspace", workspace.join(".arsy/config.toml")));
    layers
}

#[cfg(target_os = "linux")]
fn enterprise_config() -> Option<PathBuf> {
    Some(PathBuf::from("/etc/arsy/config.toml"))
}

#[cfg(target_os = "macos")]
fn enterprise_config() -> Option<PathBuf> {
    Some(PathBuf::from(
        "/Library/Application Support/ARSY/config.toml",
    ))
}

#[cfg(target_os = "windows")]
fn enterprise_config() -> Option<PathBuf> {
    std::env::var_os("ProgramData").map(|base| Path::new(&base).join("ARSY/config.toml"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn enterprise_config() -> Option<PathBuf> {
    None
}

#[cfg(target_os = "linux")]
fn user_config() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
        .map(|base| base.join("arsy/config.toml"))
}

#[cfg(target_os = "macos")]
fn user_config() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| Path::new(&home).join("Library/Application Support/ARSY/config.toml"))
}

#[cfg(target_os = "windows")]
fn user_config() -> Option<PathBuf> {
    std::env::var_os("AppData").map(|base| Path::new(&base).join("ARSY/config.toml"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn user_config() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        event::{EventPayload, EventStore},
        protocol::{ClientRequest, ProtocolEnvelope, TurnStart},
    };

    #[cfg(all(feature = "tui", unix))]
    #[test]
    fn interactive_provider_cancellation_and_terminal_failures_are_bounded() {
        use std::time::{Duration, Instant};
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
            false,
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
            false,
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
        assert_eq!(Diagnostic::error("ARSY-PRV-1000", "", "").exit_code(), 5);
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
