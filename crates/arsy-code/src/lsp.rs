//! Bounded LSP lifecycle and JSON-RPC transport.

use arsy_kernel::domain::StateVersion;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
    time::Duration,
};

pub const MAX_LSP_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_LSP_BATCH: usize = 64;

/// A semantic fact set belongs to exactly one server. Callers must choose a
/// provider explicitly instead of merging conflicting answers.
pub fn select_provider<'a, T>(
    providers: &'a BTreeMap<String, T>,
    selected: &str,
) -> Result<&'a T, LspError> {
    providers
        .get(selected)
        .ok_or_else(|| LspError::UnknownProvider(selected.to_owned()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandOrigin {
    Installed,
    Repository,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerCommand {
    pub argv: Vec<String>,
    pub origin: CommandOrigin,
    pub policy_authorized: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Overlay {
    pub version: u64,
    pub text: String,
    pub disk_revision: StateVersion,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspRequest {
    pub method: String,
    pub params: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerState {
    Stopped,
    Running,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestartPolicy {
    pub delays: Vec<Duration>,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            delays: vec![
                Duration::from_millis(50),
                Duration::from_millis(250),
                Duration::from_secs(1),
            ],
        }
    }
}

pub trait LspTransport: Send {
    fn start(&mut self, command: &ServerCommand) -> Result<(), LspError>;
    fn stop(&mut self);
    fn request_batch(&mut self, requests: &[LspRequest]) -> Result<Vec<Value>, LspError>;

    /// Send something the server will not answer.
    ///
    /// Half of LSP is notifications — `initialized`, `textDocument/didOpen` —
    /// and waiting for a response to one would hang until the server said
    /// something unrelated. Defaulted so a scripted transport in a test can
    /// ignore them.
    fn notify(&mut self, _method: &str, _params: Value) -> Result<(), LspError> {
        Ok(())
    }

    /// What the server said it can do, once the handshake is done.
    fn capabilities(&self) -> Value {
        Value::Null
    }
}

pub struct LspHost<T> {
    command: ServerCommand,
    transport: T,
    restart: RestartPolicy,
    state: ServerState,
    overlays: BTreeMap<String, Overlay>,
    queued: VecDeque<Vec<LspRequest>>,
}

impl<T: LspTransport> LspHost<T> {
    pub fn new(command: ServerCommand, transport: T, restart: RestartPolicy) -> Self {
        Self {
            command,
            transport,
            restart,
            state: ServerState::Stopped,
            overlays: BTreeMap::new(),
            queued: VecDeque::new(),
        }
    }

    pub const fn state(&self) -> ServerState {
        self.state
    }

    pub fn start(&mut self) -> Result<(), LspError> {
        if self.command.argv.first().is_none_or(String::is_empty) {
            return Err(LspError::InvalidCommand);
        }
        if self.command.origin == CommandOrigin::Repository && !self.command.policy_authorized {
            return Err(LspError::PolicyRequired);
        }
        self.transport.start(&self.command)?;
        self.state = ServerState::Running;
        Ok(())
    }

    pub fn set_overlay(
        &mut self,
        uri: impl Into<String>,
        overlay: Overlay,
    ) -> Result<(), LspError> {
        let uri = uri.into();
        if uri.is_empty()
            || self
                .overlays
                .get(&uri)
                .is_some_and(|current| overlay.version <= current.version)
        {
            return Err(LspError::StaleOverlay);
        }
        self.overlays.insert(uri, overlay);
        Ok(())
    }

    /// Send a notification to the server, starting it if it is not running.
    pub fn notify(&mut self, method: &str, params: Value) -> Result<(), LspError> {
        if self.state == ServerState::Stopped {
            self.start()?;
        }
        self.transport.notify(method, params)
    }

    /// What the server said it can do. `Null` before the handshake.
    pub fn capabilities(&self) -> Value {
        self.transport.capabilities()
    }

    pub fn overlay(&self, uri: &str) -> Option<&Overlay> {
        self.overlays.get(uri)
    }

    /// Requests stay queued until either the same batch succeeds after a
    /// bounded restart or the server is marked failed.
    pub fn request_batch(&mut self, requests: Vec<LspRequest>) -> Result<Vec<Value>, LspError> {
        if requests.is_empty() || requests.len() > MAX_LSP_BATCH {
            return Err(LspError::InvalidBatch);
        }
        self.queued.push_back(requests);
        if self.state == ServerState::Stopped {
            self.start()?;
        }

        let mut attempt = 0;
        loop {
            let current = self.queued.front().expect("request was queued");
            match self.transport.request_batch(current) {
                Ok(values) => {
                    self.queued.pop_front();
                    return Ok(values);
                }
                Err(LspError::Crashed) if attempt < self.restart.delays.len() => {
                    self.state = ServerState::Stopped;
                    self.transport.stop();
                    thread::sleep(self.restart.delays[attempt]);
                    attempt += 1;
                    self.start()?;
                }
                Err(error) => {
                    self.state = ServerState::Failed;
                    self.queued.pop_front();
                    return Err(error);
                }
            }
        }
    }
}

impl<T> Drop for LspHost<T> {
    fn drop(&mut self) {
        // The concrete transport owns process cleanup.
    }
}

pub struct StdioTransport {
    child: Option<Child>,
    input: Option<ChildStdin>,
    output: Option<BufReader<ChildStdout>>,
    next_id: u64,
    environment: BTreeMap<String, String>,
    working_directory: Option<PathBuf>,
    /// The workspace the server is opened on. A server initialized without a
    /// root indexes nothing and answers `workspace/symbol` with nothing.
    root: Option<PathBuf>,
    capabilities: Value,
}

impl StdioTransport {
    pub fn new(environment: BTreeMap<String, String>, working_directory: Option<PathBuf>) -> Self {
        Self {
            child: None,
            input: None,
            output: None,
            next_id: 1,
            environment,
            working_directory,
            root: None,
            capabilities: Value::Null,
        }
    }

    /// The workspace root the server is told to index.
    #[must_use]
    pub fn rooted_at(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// `initialize`, then `initialized`.
    ///
    /// A server answers nothing before this, and answers `workspace/symbol`
    /// with an empty list until it has finished indexing — which it will not
    /// begin until it is told where the workspace is.
    fn handshake(&mut self) -> Result<(), LspError> {
        let root = self
            .root
            .clone()
            .or_else(|| self.working_directory.clone())
            .unwrap_or_else(|| PathBuf::from("."));
        let result = self.request(&LspRequest {
            method: "initialize".into(),
            params: json!({
                "processId": std::process::id(),
                "clientInfo": {"name": "arsy", "version": env!("CARGO_PKG_VERSION")},
                "rootUri": file_uri(&root),
                "workspaceFolders": [{"uri": file_uri(&root), "name": "workspace"}],
                "capabilities": {
                    "workspace": {
                        "symbol": {"dynamicRegistration": false},
                        "workspaceEdit": {"documentChanges": false},
                        "workspaceFolders": true,
                    },
                    "textDocument": {
                        "synchronization": {"dynamicRegistration": false},
                        "hover": {"contentFormat": ["plaintext", "markdown"]},
                        "references": {"dynamicRegistration": false},
                        "rename": {"dynamicRegistration": false, "prepareSupport": false},
                        "diagnostic": {"dynamicRegistration": false},
                        // Byte offsets are what everything inside ARSY uses, and
                        // UTF-8 is the only encoding that makes a character
                        // offset convertible to one without guessing. A server
                        // that refuses stays on UTF-16 and is converted.
                        "publishDiagnostics": {"relatedInformation": false},
                    },
                    "general": {"positionEncodings": ["utf-8", "utf-16"]},
                },
            }),
        })?;
        self.capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
        self.notify("initialized", json!({}))
    }

    fn request(&mut self, request: &LspRequest) -> Result<Value, LspError> {
        if self
            .child
            .as_mut()
            .ok_or(LspError::Crashed)?
            .try_wait()
            .map_err(LspError::Io)?
            .is_some()
        {
            return Err(LspError::Crashed);
        }
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(LspError::IdOverflow)?;
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": request.method,
            "params": request.params,
        }))
        .map_err(|error| LspError::Protocol(error.to_string()))?;
        if body.len() > MAX_LSP_MESSAGE_BYTES {
            return Err(LspError::MessageTooLarge);
        }
        let input = self.input.as_mut().ok_or(LspError::Crashed)?;
        write!(input, "Content-Length: {}\r\n\r\n", body.len()).map_err(LspError::Io)?;
        input.write_all(&body).map_err(LspError::Io)?;
        input.flush().map_err(LspError::Io)?;

        loop {
            let response = read_message(self.output.as_mut().ok_or(LspError::Crashed)?)?;
            if response.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = response.get("error") {
                    return Err(LspError::Protocol(error.to_string()));
                }
                return response
                    .get("result")
                    .cloned()
                    .ok_or_else(|| LspError::Protocol("response has no result".into()));
            }
        }
    }
}

impl LspTransport for StdioTransport {
    fn start(&mut self, server: &ServerCommand) -> Result<(), LspError> {
        self.stop();
        let (program, args) = server.argv.split_first().ok_or(LspError::InvalidCommand)?;
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .envs(&self.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(directory) = &self.working_directory {
            command.current_dir(directory);
        }
        let mut child = command.spawn().map_err(LspError::Io)?;
        self.input = child.stdin.take();
        self.output = child.stdout.take().map(BufReader::new);
        self.child = Some(child);
        self.handshake()
    }

    fn stop(&mut self) {
        self.input.take();
        self.output.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn request_batch(&mut self, requests: &[LspRequest]) -> Result<Vec<Value>, LspError> {
        requests
            .iter()
            .map(|request| self.request(request))
            .collect()
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), LspError> {
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .map_err(|error| LspError::Protocol(error.to_string()))?;
        if body.len() > MAX_LSP_MESSAGE_BYTES {
            return Err(LspError::MessageTooLarge);
        }
        let input = self.input.as_mut().ok_or(LspError::Crashed)?;
        write!(input, "Content-Length: {}\r\n\r\n", body.len()).map_err(LspError::Io)?;
        input.write_all(&body).map_err(LspError::Io)?;
        input.flush().map_err(LspError::Io)
    }

    fn capabilities(&self) -> Value {
        self.capabilities.clone()
    }
}

/// A path as the `file:` URI every LSP server expects.
///
/// Percent-encoding is deliberately minimal: the characters that would change
/// how a URI parses, and nothing else. A path is not a URL and re-encoding one
/// as though it were is how servers end up opening the wrong file.
pub fn file_uri(path: &Path) -> String {
    let mut encoded = String::from("file://");
    let display = path.to_string_lossy().replace('\\', "/");
    if !display.starts_with('/') {
        encoded.push('/');
    }
    for character in display.chars() {
        match character {
            '%' => encoded.push_str("%25"),
            ' ' => encoded.push_str("%20"),
            '#' => encoded.push_str("%23"),
            '?' => encoded.push_str("%3F"),
            other => encoded.push(other),
        }
    }
    encoded
}

/// The path a `file:` URI names, undoing [`file_uri`].
///
/// Decoded into bytes and then read as UTF-8, not into `char`s. A percent
/// escape is a byte, and a non-ASCII character is several of them: decoding
/// each escape to `char::from(byte)` would read them as Latin-1 and turn
/// `caf%C3%A9` into `cafÃ©`. ARSY's own `file_uri` escapes almost nothing, so
/// a round trip through it never noticed — but every URI decoded here came
/// from a language server, and rust-analyzer, gopls, and clangd all encode
/// non-ASCII. A rename touching such a path would then fail to read the file
/// it had just planned edits for.
pub fn uri_path(uri: &str) -> PathBuf {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let raw = rest.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' && index + 2 < raw.len() {
            let hex = std::str::from_utf8(&raw[index + 1..index + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            if let Some(byte) = hex {
                decoded.push(byte);
                index += 3;
                continue;
            }
        }
        // Not an escape after all; keep what was written rather than dropping
        // it.
        decoded.push(raw[index]);
        index += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&decoded).into_owned())
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.stop();
    }
}

fn read_message(reader: &mut impl BufRead) -> Result<Value, LspError> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(LspError::Io)? == 0 {
            return Err(LspError::Crashed);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| LspError::Protocol("invalid content length".into()))?,
            );
        }
    }
    let length = length.ok_or_else(|| LspError::Protocol("missing content length".into()))?;
    if length > MAX_LSP_MESSAGE_BYTES {
        return Err(LspError::MessageTooLarge);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(LspError::Io)?;
    serde_json::from_slice(&body).map_err(|error| LspError::Protocol(error.to_string()))
}

#[derive(Debug)]
pub enum LspError {
    InvalidCommand,
    PolicyRequired,
    StaleOverlay,
    InvalidBatch,
    MessageTooLarge,
    IdOverflow,
    Crashed,
    UnknownProvider(String),
    Protocol(String),
    Io(io::Error),
}

impl fmt::Display for LspError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCommand => formatter.write_str("language server command is empty"),
            Self::PolicyRequired => {
                formatter.write_str("repository language server command requires policy approval")
            }
            Self::StaleOverlay => formatter.write_str("overlay version did not advance"),
            Self::InvalidBatch => formatter.write_str("LSP batch is empty or exceeds its cap"),
            Self::MessageTooLarge => formatter.write_str("LSP message exceeds its byte cap"),
            Self::IdOverflow => formatter.write_str("LSP request id overflow"),
            Self::Crashed => formatter.write_str("language server exited"),
            Self::UnknownProvider(provider) => {
                write!(
                    formatter,
                    "language server provider {provider} was not selected"
                )
            }
            Self::Protocol(message) => write!(formatter, "LSP protocol error: {message}"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for LspError {}

#[cfg(test)]
mod tests {
    use super::*;

    struct CrashOnce {
        starts: usize,
        crashed: bool,
    }

    impl LspTransport for CrashOnce {
        fn start(&mut self, _command: &ServerCommand) -> Result<(), LspError> {
            self.starts += 1;
            Ok(())
        }

        fn stop(&mut self) {}

        fn request_batch(&mut self, requests: &[LspRequest]) -> Result<Vec<Value>, LspError> {
            if !self.crashed {
                self.crashed = true;
                Err(LspError::Crashed)
            } else {
                Ok(requests
                    .iter()
                    .map(|request| request.params.clone())
                    .collect())
            }
        }
    }

    fn command(origin: CommandOrigin, authorized: bool) -> ServerCommand {
        ServerCommand {
            argv: vec!["server".into()],
            origin,
            policy_authorized: authorized,
        }
    }

    #[test]
    fn crash_restarts_with_a_bound_and_replays_the_queued_batch() {
        let transport = CrashOnce {
            starts: 0,
            crashed: false,
        };
        let mut host = LspHost::new(
            command(CommandOrigin::Installed, false),
            transport,
            RestartPolicy {
                delays: vec![Duration::ZERO],
            },
        );
        let response = host
            .request_batch(vec![LspRequest {
                method: "workspace/symbol".into(),
                params: json!({"query": "Thing"}),
            }])
            .unwrap();

        assert_eq!(response, vec![json!({"query": "Thing"})]);
        assert_eq!(host.transport.starts, 2);
        assert_eq!(host.state(), ServerState::Running);
    }

    #[test]
    fn repository_commands_need_policy_and_overlay_versions_are_distinct() {
        let transport = CrashOnce {
            starts: 0,
            crashed: true,
        };
        let mut host = LspHost::new(
            command(CommandOrigin::Repository, false),
            transport,
            RestartPolicy::default(),
        );
        assert!(matches!(host.start(), Err(LspError::PolicyRequired)));

        host.set_overlay(
            "file:///repo/a.rs",
            Overlay {
                version: 2,
                text: "unsaved".into(),
                disk_revision: StateVersion::from_digest([1; 32]),
            },
        )
        .unwrap();
        assert!(matches!(
            host.set_overlay(
                "file:///repo/a.rs",
                Overlay {
                    version: 2,
                    text: "stale".into(),
                    disk_revision: StateVersion::from_digest([2; 32]),
                }
            ),
            Err(LspError::StaleOverlay)
        ));
        assert_eq!(host.overlay("file:///repo/a.rs").unwrap().text, "unsaved");
    }

    #[test]
    fn conflicting_servers_require_an_explicit_provider() {
        let providers = BTreeMap::from([
            ("rust-analyzer".to_owned(), "definition-a"),
            ("other-server".to_owned(), "definition-b"),
        ]);
        assert_eq!(
            select_provider(&providers, "rust-analyzer").unwrap(),
            &"definition-a"
        );
        assert!(matches!(
            select_provider(&providers, "missing"),
            Err(LspError::UnknownProvider(_))
        ));
    }
}
