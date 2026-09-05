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

mod eval;
pub mod provider;
#[cfg(feature = "tui")]
pub mod tui;

use arsy_kernel::{
    domain::{Principal, SessionId},
    event::EventStore,
    protocol::{ClientRequest, Extensions, IdempotencyKey, ProtocolEnvelope, TurnStart},
    provider::{
        CanonicalModelRequest, ModelContent, ModelEvent, ModelKey, ModelMessage, ModelProvider,
        ModelRole, ProviderError,
    },
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

/// No provider credential is available, so the turn cannot dispatch.
pub const ARSY_PRV_1000: &str = "ARSY-PRV-1000";
/// A configuration layer could not be read or does not parse.
pub const ARSY_CFG_1000: &str = "ARSY-CFG-1000";
/// Machine records carry the protocol's schema version.
const RECORD_SCHEMA: u32 = 1;

/// Documented commands that a later phase ships, so they report their phase
/// instead of failing as unknown input.
const UNAVAILABLE: &[(&str, u8)] = &[
    ("artifact", 1),
    ("completions", 1),
    ("gc", 1),
    ("hook", 8),
    ("mcp", 5),
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
  arsy config explain [KEY]  show effective configuration and where it came from
  arsy auth set <PROVIDER>   store a credential in the OS credential store
  arsy auth login <PROVIDER> sign in to a provider through its OAuth client
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
        out: Option<PathBuf>,
    },
    CompatExplain {
        ecosystem: arsy_code::compat::Ecosystem,
    },
    /// `arsy config explain [KEY]`: effective values and where each came from.
    ConfigExplain {
        key: Option<String>,
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
        Some("config") => parse_config(parsed.positional)?,
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
            // Both human and CI keep diagnostics on stderr; the CI form is the
            // stable, unlocalized one machines grep for.
            _ => {
                let severity = diagnostic.severity.as_str().to_uppercase();
                let _ = writeln!(
                    io::stderr(),
                    "ARSY {severity} {} {}\n  {}",
                    diagnostic.code,
                    message,
                    remediation
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

    /// One chunk of streamed model text.
    ///
    /// Human output writes it straight through so a reply appears as it is
    /// produced; machine output makes each chunk its own record, because a
    /// JSON-lines consumer cannot read a partial line.
    fn delta(&mut self, text: &str) {
        match self.output {
            Output::Json => self.record("model.delta", json!({"text": text})),
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
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
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
        Command::AuthLogin { provider } => auth_login(invocation, provider, emitter),
        Command::AuthList => auth_list(emitter),
        Command::AuthRemove { handle, force } => auth_remove(handle, *force, emitter),
        Command::Eval { suite, trials, out } => {
            let workspace = workspace_root(&invocation.workspace)?;
            let report = eval::run(&workspace, suite, *trials, out.as_deref())?;
            emitter.result(serde_json::to_value(report).map_err(storage_failed)?);
            Ok(0)
        }
        Command::CompatExplain { ecosystem } => compat_explain(invocation, *ecosystem, emitter),
        Command::ConfigExplain { key } => config_explain(invocation, key.as_deref(), emitter),
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

fn config_explain(
    invocation: &Invocation,
    key: Option<&str>,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    emitter.result(load_config(&root, &working)?.explain(key));
    Ok(0)
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
    let endpoint = config.endpoint(Some(provider)).ok_or_else(|| {
        Diagnostic::error(
            ARSY_PRV_1000,
            format!("no provider endpoint named `{provider}` is configured"),
            "add a `[provider.endpoint.<name>]` table to the user config.toml",
        )
    })?;
    let oauth = endpoint.oauth.as_ref().ok_or_else(|| {
        Diagnostic::error(
            ARSY_PRV_1000,
            format!("provider `{provider}` has no OAuth client configured"),
            format!(
                "add a `[provider.endpoint.{provider}.oauth]` table, or store an API key \
                 with `arsy auth set {provider}`"
            ),
        )
    })?;

    let transport = arsy_kernel::provider::http::HttpTransport::default();
    let tokens = if arsy_kernel::oauth::uses_device_grant(oauth) {
        let prompt = arsy_kernel::oauth::begin_device(&transport, oauth).map_err(login_failed)?;
        // Printed rather than opened: the operator may be on another machine,
        // and this is the grant that does not need a local browser at all.
        emitter.result(json!({
            "provider": provider,
            "verification_uri": prompt.verification_uri_complete
                .clone()
                .unwrap_or_else(|| prompt.verification_uri.clone()),
            "user_code": prompt.user_code,
        }));
        arsy_kernel::oauth::poll_device(&transport, oauth, &prompt, &mut std::thread::sleep)
            .map_err(login_failed)?
    } else {
        let mut url = None;
        let tokens = arsy_kernel::oauth::authorization_code(&transport, oauth, &mut |authorize| {
            url = Some(authorize.to_owned());
            let _ = writeln!(io::stderr(), "Open this URL to sign in:\n  {authorize}");
        });
        tokens.map_err(login_failed)?
    };

    let handle = SecretHandle::new(OS_STORE_ID, provider).map_err(secret_failed)?;
    let raw = serde_json::to_string(&tokens).map_err(|error| secret_failed(error.to_string()))?;
    let store = OsCredentialStore;
    store.set(handle.name(), &raw).map_err(secret_failed)?;
    let mut records = catalog(store)?;
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
    save_catalog(store, &records)?;
    emitter.result(json!({
        "provider": provider,
        "handle": handle,
        "kind": "oauth",
        "expires_at": tokens.expires_at,
    }));
    Ok(0)
}

fn login_failed(error: arsy_kernel::oauth::OAuthError) -> Diagnostic {
    Diagnostic::error(
        ARSY_PRV_1000,
        error.to_string(),
        "check the OAuth client in `arsy config explain`, then run `arsy auth login` again",
    )
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

    // A configured endpoint is preferred, because it is the one ARSY talks to
    // itself. The Codex CLI stays the fallback for an operator who has not
    // configured anything, so this session keeps working as it did.
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
    let Some(detected) = detected else {
        return Err(Diagnostic::error(
            ARSY_PRV_1000,
            "no provider is available: nothing is configured, and no logged-in Codex CLI was \
             detected",
            "configure a `[provider.endpoint.<name>]` table and run `arsy auth set <name>`, or \
             install Codex and run `codex login`",
        ));
    };
    let native = native.map(|(resolved, _)| resolved);
    let colour = !invocation.no_color && std::env::var_os("NO_COLOR").is_none();

    // The model picker lists what the Codex CLI cached for its account, which
    // says nothing about a configured endpoint; there, the picker takes a slug
    // as free text.
    let models = if detected.is_codex() {
        tui::available_models()
    } else {
        Vec::new()
    };
    // A remembered route only applies to the provider it was chosen for.
    let remembered = saved_route().filter(|saved| saved.provider == detected.provider);
    let mut route = remembered.clone().unwrap_or(detected);

    let mut state = tui::TuiState::new(workspace.display().to_string(), SessionId::new());
    state.set_model_route(route.clone());
    writeln!(stdout, "{}", state.render(tui::terminal_width(), colour)).map_err(terminal_failed)?;

    // ARSY paints the input line from here on, so it owns the terminal modes
    // and is the only reader of stdin.
    let _raw = tui::RawTerminal::acquire();
    let keys = tui::spawn_key_reader();
    let mut decoder = tui::Keys::default();
    let mut composer = tui::Composer::default();

    // A remembered model skips the picker; `/model` reopens it.
    // A line submitted while a turn was running runs next, before stdin is
    // read again.
    let mut queued: Option<String> = None;
    let mut prompt = if remembered.is_some() || !route.model.is_empty() {
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
        let line = match queued.take() {
            Some(line) => line,
            None => {
                let Some(line) = read_line(
                    &keys,
                    &mut decoder,
                    &mut composer,
                    &mut stdout,
                    colour,
                    &status,
                )?
                else {
                    break;
                };
                line
            }
        };
        match prompt {
            Prompt::Model => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                route = tui::resolve_model(&line, &models, &route);
                remember_model(&route, emitter);
                state.set_model_route(route.clone());
                prompt = Prompt::Task;
            }
            Prompt::Task if line.trim() == "/model" => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
                tui::render_model_list(&mut stdout, &models, &route, colour)
                    .map_err(terminal_failed)?;
                prompt = Prompt::Model;
            }
            Prompt::Task if line.trim() == ":quit" => break,
            Prompt::Task if line.trim().is_empty() => {
                write!(stdout, "{}", composer.clear()).map_err(terminal_failed)?;
            }
            Prompt::Task => {
                write!(stdout, "{}", composer.commit(&line, colour)).map_err(terminal_failed)?;
                stdout.flush().map_err(terminal_failed)?;
                match run_turn(
                    invocation,
                    native.as_ref(),
                    &line,
                    &route,
                    colour,
                    &keys,
                    &mut decoder,
                    &mut composer,
                    emitter,
                ) {
                    Ok(turn) if turn.quit => break,
                    Ok(turn) => queued = turn.queued,
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
    loop {
        write!(
            stdout,
            "{}",
            composer.render(tui::terminal_width(), colour, status)
        )
        .map_err(terminal_failed)?;
        stdout.flush().map_err(terminal_failed)?;
        loop {
            // The timeout is what tells a lone Escape apart from the start of
            // an arrow-key sequence.
            let key = match keys.recv_timeout(std::time::Duration::from_millis(40)) {
                Ok(byte) => decoder.feed(byte),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => decoder.flush_escape(),
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

/// The remembered model lives beside the user configuration layer that
/// `arsy doctor` already reports.
#[cfg(feature = "tui")]
fn model_store() -> Option<PathBuf> {
    Some(arsy_kernel::config::user_config()?.with_file_name("model"))
}

#[cfg(feature = "tui")]
/// The route chosen last time, as `provider/model`.
fn saved_route() -> Option<tui::ModelRoute> {
    let raw = std::fs::read_to_string(model_store()?).ok()?;
    let raw = raw.trim();
    (!raw.is_empty()).then(|| tui::ModelRoute::parse(raw))
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

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn run_turn(
    invocation: &Invocation,
    native: Option<&provider::Resolved>,
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
    let outcome = match native.filter(|_| !route.is_codex()) {
        Some(resolved) => native_status(
            resolved,
            &task,
            route,
            admission.turn,
            colour,
            keys,
            decoder,
            composer,
        ),
        None => external_status(
            &workspace_root(&invocation.workspace)?,
            &task,
            route,
            colour,
            keys,
            decoder,
            composer,
        ),
    };
    let turn = match outcome {
        Ok(turn) => turn,
        Err(error) => {
            fail_turn(
                &service,
                actor,
                admission.turn,
                session,
                route,
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
    match &turn.failure {
        None => {
            let mut outcome = json!({"provider": route.provider, "model": route.model});
            merge(&mut outcome, turn.usage.clone());
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
        Some(failure) => {
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

/// Stream one turn from a configured provider, keeping the composer alive.
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
    task: &str,
    route: &tui::ModelRoute,
    turn: arsy_kernel::domain::TurnId,
    colour: bool,
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
) -> io::Result<Turn> {
    let request = CanonicalModelRequest {
        model: ModelKey {
            provider: route.provider.clone(),
            model: route.model.clone(),
        },
        system: None,
        messages: vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: task.to_owned(),
            }],
        }],
        // As in `arsy run`: operations are not dispatched from here yet, so a
        // tool offered now would have nowhere to run.
        tools: Vec::new(),
        max_output_tokens: resolved.endpoint.max_output_tokens,
        idempotency_key: arsy_kernel::protocol::IdempotencyKey::new(turn.to_string())
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
                Ok(ModelEvent::Usage {
                    input_tokens,
                    output_tokens,
                }) => Ok(Streamed::Usage {
                    input_tokens,
                    output_tokens,
                }),
                Ok(ModelEvent::ToolCallCompleted { name, .. }) => Err(format!(
                    "the model called the tool `{name}`, which this path cannot run yet"
                )),
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
    let working_status = tui::working_row(colour);
    // Deltas arrive token by token; a row is emitted per line so scrollback
    // reads like the Codex projection rather than one row per token.
    let mut pending = String::new();
    let draw = |terminal: &mut io::Stdout, composer: &mut tui::Composer, row: Option<&str>| {
        let mut frame = composer.clear();
        if let Some(row) = row {
            frame.push_str(row);
            frame.push('\n');
        }
        frame.push_str(&composer.render(tui::terminal_width(), colour, &working_status));
        write!(terminal, "{frame}").and_then(|()| terminal.flush())
    };
    draw(&mut terminal, composer, None)?;
    loop {
        let mut typed = false;
        while let Ok(byte) = keys.try_recv() {
            let Some(key) = decoder.feed(byte) else {
                continue;
            };
            if key == tui::Key::Interrupt {
                outcome.queued = None;
                outcome.interrupted = true;
                draw(&mut terminal, composer, Some(&tui::interrupted_row(colour)))?;
                return finish(terminal, composer, outcome);
            }
            match composer.press(key) {
                tui::Action::Submit(line) => outcome.queued = Some(line),
                tui::Action::Quit => outcome.quit = true,
                tui::Action::Redraw => typed = true,
                tui::Action::None => {}
            }
        }
        if typed {
            draw(&mut terminal, composer, None)?;
        }
        match events.recv_timeout(std::time::Duration::from_millis(40)) {
            Ok(Ok(Streamed::Text(text))) => {
                pending.push_str(&text);
                while let Some(newline) = pending.find('\n') {
                    let line: String = pending.drain(..=newline).collect();
                    draw(
                        &mut terminal,
                        composer,
                        Some(&tui::assistant_row(colour, &line)),
                    )?;
                }
            }
            Ok(Ok(Streamed::Usage {
                input_tokens,
                output_tokens,
            })) => {
                outcome.usage =
                    json!({"input_tokens": input_tokens, "output_tokens": output_tokens});
            }
            Ok(Err(failure)) => {
                outcome.failure = Some(failure);
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if decoder.flush_escape() == Some(tui::Key::Interrupt) {
                    outcome.interrupted = true;
                    draw(&mut terminal, composer, Some(&tui::interrupted_row(colour)))?;
                    return finish(terminal, composer, outcome);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if !pending.trim().is_empty() {
        draw(
            &mut terminal,
            composer,
            Some(&tui::assistant_row(colour, &pending)),
        )?;
    }
    finish(terminal, composer, outcome)
}

/// One streamed fact from a provider, as the terminal needs it.
#[cfg(feature = "tui")]
enum Streamed {
    Text(String),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
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
    keys: &std::sync::mpsc::Receiver<u8>,
    decoder: &mut tui::Keys,
    composer: &mut tui::Composer,
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
    let mut child = command
        .current_dir(workspace)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // `--json` reports failures as `error` events, so the human-formatted
        // copy on stderr would only duplicate them inside the rendered turn.
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let Err(error) = child
        .stdin
        .take()
        .expect("piped stdin is available")
        .write_all(task.as_bytes())
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    // The event stream is read on a thread so the main loop can also watch the
    // key stream: that is what lets Esc or Ctrl-C stop a turn, and what keeps
    // the composer alive and typeable while the provider works.
    let stdout = child.stdout.take().expect("piped stdout is available");
    let (rows, events) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in io::BufRead::lines(io::BufReader::new(stdout)) {
            let Ok(line) = line else { break };
            if rows.send(line).is_err() {
                break;
            }
        }
    });

    let pid = child.id();
    let mut outcome = Turn::default();
    let mut terminal = io::stdout();
    let working_status = tui::working_row(colour);
    let draw = |terminal: &mut io::Stdout, composer: &mut tui::Composer, row: Option<&str>| {
        // Rows land above the composer, which is torn down and repainted around
        // each one so the input block is never overwritten.
        let mut frame = composer.clear();
        if let Some(row) = row {
            frame.push_str(row);
            frame.push('\n');
        }
        frame.push_str(&composer.render(tui::terminal_width(), colour, &working_status));
        write!(terminal, "{frame}").and_then(|()| terminal.flush())
    };
    draw(&mut terminal, composer, None)?;
    loop {
        let mut typed = false;
        while let Ok(byte) = keys.try_recv() {
            let Some(key) = decoder.feed(byte) else {
                continue;
            };
            // While the provider is running, Interrupt always means the turn,
            // never the composer or the session — and it drops a queued
            // follow-up, which was only queued to run after this turn.
            if key == tui::Key::Interrupt {
                outcome.queued = None;
                if !outcome.interrupted {
                    outcome.interrupted = true;
                    terminate(pid);
                    draw(&mut terminal, composer, Some(&tui::interrupted_row(colour)))?;
                }
                continue;
            }
            match composer.press(key) {
                // A line sent while the provider is busy runs as soon as this
                // turn ends, rather than being dropped or blocking.
                tui::Action::Submit(line) => outcome.queued = Some(line),
                tui::Action::Quit => outcome.quit = true,
                tui::Action::Redraw => typed = true,
                tui::Action::None => {}
            }
        }
        if typed {
            draw(&mut terminal, composer, None)?;
        }
        match events.recv_timeout(std::time::Duration::from_millis(40)) {
            Ok(line) => {
                // A killed provider still flushes buffered events; showing them
                // after the interrupt notice would contradict it.
                if !outcome.interrupted {
                    if let Some(row) = tui::render_codex_event(&line, colour) {
                        draw(&mut terminal, composer, Some(&row))?;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if decoder.flush_escape() == Some(tui::Key::Interrupt) && !outcome.interrupted {
                    outcome.interrupted = true;
                    terminate(pid);
                    draw(&mut terminal, composer, Some(&tui::interrupted_row(colour)))?;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait()?;
    if !status.success() && !outcome.interrupted {
        outcome.failure = Some(format!("{route} exited with status {status}"));
    }
    finish(terminal, composer, outcome)
}

/// What one interactive turn left behind, whichever route ran it.
#[cfg(feature = "tui")]
#[derive(Default)]
struct Turn {
    /// `None` when the turn succeeded; otherwise why it did not.
    failure: Option<String>,
    /// Extra facts to record on a completed turn, such as token usage.
    usage: Value,
    interrupted: bool,
    /// A line submitted while this turn was still running.
    queued: Option<String>,
    quit: bool,
}

/// Ask the provider to stop. `SIGTERM` first is enough for the Codex CLI; the
/// wait that follows reaps it either way.
#[cfg(feature = "tui")]
fn terminate(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
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
    let task = prepare_task(&task, emitter)?;
    let root = workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());

    // Resolved before the turn is recorded: a misconfiguration is the
    // operator's to fix, not a failed turn in their session history.
    let resolved = load_config(&root, &working).and_then(|config| {
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
        Ok((resolved, model))
    });
    let (resolved, model) = match resolved {
        Ok(resolved) => resolved,
        Err(mut diagnostic) => {
            if diagnostic.code == ARSY_PRV_1000 {
                diagnostic.remediation = format!(
                    "{}; or use the interactive TUI with a logged-in Codex CLI",
                    diagnostic.remediation
                );
            }
            emitter.diagnostic(&diagnostic);
            return Ok(diagnostic.exit_code());
        }
    };

    let (service, actor, admission, session) = record_turn(invocation, task.clone(), emitter)?;
    let request = CanonicalModelRequest {
        model: ModelKey {
            provider: resolved.endpoint.id.clone(),
            model,
        },
        system: None,
        messages: vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text { text: task }],
        }],
        // Operations are not dispatched from this path yet, so offering tools
        // would invite calls nothing can run.
        tools: Vec::new(),
        max_output_tokens: resolved.endpoint.max_output_tokens,
        // The turn id, so a retried attempt is provably the same request.
        idempotency_key: IdempotencyKey::new(admission.turn.to_string())
            .map_err(|error| storage_failed(error.to_string()))?,
    };

    let outcome = dispatch(resolved.provider.as_ref(), &request, emitter);
    let record = json!({
        "session": session.to_string(),
        "turn": admission.turn.to_string(),
        "provider": resolved.endpoint.id,
        "model": request.model.model,
    });
    match outcome {
        Ok(usage) => {
            let mut outcome = record.clone();
            merge(&mut outcome, usage);
            service
                .complete_turn(actor, admission.turn, &outcome)
                .map_err(storage_failed)?;
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
            service
                .fail_turn(actor, admission.turn, error.code(), error.to_string())
                .map_err(storage_failed)?;
            emitter.diagnostic(&diagnostic);
            let mut result = record;
            merge(&mut result, json!({"status": "failed"}));
            emitter.result(result);
            Ok(diagnostic.exit_code())
        }
    }
}

/// Stream one turn, rendering it as it arrives, and report what it used.
///
/// A tool call cannot be honoured from this path, so one is reported rather
/// than silently dropped: a caller that sees `stop: tool_use` and no result
/// would otherwise think the model simply stopped.
fn dispatch(
    provider: &dyn ModelProvider,
    request: &CanonicalModelRequest,
    emitter: &mut Emitter,
) -> Result<Value, ProviderError> {
    let mut usage = json!({});
    let stream =
        arsy_kernel::provider::stream_with_retry(provider, request, &mut std::thread::sleep)?;
    for event in stream {
        match event? {
            ModelEvent::TextDelta { text } => emitter.delta(&text),
            ModelEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                usage = json!({"input_tokens": input_tokens, "output_tokens": output_tokens});
            }
            ModelEvent::ToolCallCompleted { name, .. } => {
                return Err(ProviderError::InvalidRequest(format!(
                    "the model called the tool `{name}`, which this path cannot run yet"
                )))
            }
            ModelEvent::Completed { .. }
            | ModelEvent::ToolCallStarted { .. }
            | ModelEvent::ToolCallDelta { .. } => {}
        }
    }
    emitter.end_deltas();
    Ok(usage)
}

/// Fold `extra`'s fields into `target`, which is always an object here.
fn merge(target: &mut Value, extra: Value) {
    if let (Some(target), Some(extra)) = (target.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
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
