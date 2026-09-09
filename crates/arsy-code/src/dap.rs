use arsy_kernel::{
    capability::CapabilityAction,
    domain::{ResourceRef, SessionId},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fmt,
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};

const MAX_DAP_EVIDENCE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DapCapabilities {
    pub attach: bool,
    pub configuration_done: bool,
    pub evaluate: bool,
    pub read_memory: bool,
    pub write_memory: bool,
    pub terminate: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DebugStart {
    Launch,
    Attach,
}

impl DebugStart {
    const fn action(self) -> CapabilityAction {
        match self {
            Self::Launch => CapabilityAction::DebugLaunch,
            Self::Attach => CapabilityAction::DebugAttach,
        }
    }

    const fn command(self) -> &'static str {
        match self {
            Self::Launch => "launch",
            Self::Attach => "attach",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", content = "arguments", rename_all = "snake_case")]
pub enum DebugOperation {
    SetBreakpoints(Value),
    /// Sent once after the breakpoints, when the adapter negotiated it: it is
    /// what tells the adapter the client is done configuring and execution may
    /// begin.
    ConfigurationDone(Value),
    Continue(Value),
    Step(Value),
    StackTrace(Value),
    Scopes(Value),
    Variables(Value),
    Evaluate(Value),
    ReadMemory(Value),
    WriteMemory(Value),
}

impl DebugOperation {
    fn command(&self) -> &'static str {
        match self {
            Self::SetBreakpoints(_) => "setBreakpoints",
            Self::ConfigurationDone(_) => "configurationDone",
            Self::Continue(_) => "continue",
            Self::Step(_) => "next",
            Self::StackTrace(_) => "stackTrace",
            Self::Scopes(_) => "scopes",
            Self::Variables(_) => "variables",
            Self::Evaluate(_) => "evaluate",
            Self::ReadMemory(_) => "readMemory",
            Self::WriteMemory(_) => "writeMemory",
        }
    }

    fn arguments(&self) -> &Value {
        match self {
            Self::SetBreakpoints(value)
            | Self::ConfigurationDone(value)
            | Self::Continue(value)
            | Self::Step(value)
            | Self::StackTrace(value)
            | Self::Scopes(value)
            | Self::Variables(value)
            | Self::Evaluate(value)
            | Self::ReadMemory(value)
            | Self::WriteMemory(value) => value,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugEvidence {
    pub session: SessionId,
    pub artifact: ResourceRef,
    pub command: String,
    /// The adapter's answer.
    ///
    /// The artifact is the receipt and this is the result: a caller that has
    /// to act on where the program stopped cannot do it by reading an artifact
    /// id, and re-requesting the same thing to see it would ask a debugger the
    /// same question twice.
    pub body: Value,
}

pub trait DapTransport {
    fn initialize(&mut self) -> Result<DapCapabilities, DapError>;
    fn request(&mut self, command: &str, arguments: Value) -> Result<Value, DapError>;

    /// Events the adapter sent that no request asked for.
    ///
    /// A debug adapter's most important message is unsolicited: `stopped` is
    /// how "you hit your breakpoint" arrives. A transport that only correlated
    /// responses would be unable to say why execution stopped, or whether it
    /// did. Defaulted, because a scripted transport in a test has none.
    fn drain_events(&mut self) -> Vec<Value> {
        Vec::new()
    }

    /// Wait for whichever of `events` arrives first, up to `deadline`.
    ///
    /// The event that matters most arrives *after* the request that caused it:
    /// an adapter answers `continue` at once and reports `stopped` when the
    /// program actually stops, which may be a second later. A client that only
    /// looked at what had already arrived would decide the program never
    /// stopped.
    ///
    /// Several names, because the alternatives have to be waited for together:
    /// a program that exits without hitting a breakpoint sends `terminated`
    /// and never sends `stopped`, and waiting out the deadline to learn that
    /// would make every such run take as long as the deadline. `None` means
    /// the deadline passed or the adapter went quiet.
    fn wait_for_event(
        &mut self,
        _events: &[&str],
        _deadline: Duration,
    ) -> Result<Option<Value>, DapError> {
        Ok(None)
    }
}

pub trait DebugEvidenceSink {
    fn capture(&mut self, value: &Value) -> Result<ResourceRef, DapError>;
}

pub struct DapHost<T, S> {
    transport: T,
    sink: S,
    capabilities: DapCapabilities,
    session: Option<(SessionId, u64)>,
}

impl<T: DapTransport, S: DebugEvidenceSink> DapHost<T, S> {
    pub fn connect(mut transport: T, sink: S) -> Result<Self, DapError> {
        let capabilities = transport.initialize()?;
        Ok(Self {
            transport,
            sink,
            capabilities,
            session: None,
        })
    }

    pub fn start(
        &mut self,
        kind: DebugStart,
        arguments: Value,
        grants: &BTreeSet<CapabilityAction>,
        lease_expires_at_ms: u64,
    ) -> Result<DebugEvidence, DapError> {
        if !grants.contains(&kind.action()) {
            return Err(DapError::PolicyRequired(kind.action()));
        }
        if kind == DebugStart::Attach && !self.capabilities.attach {
            return Err(DapError::Unsupported("attach"));
        }
        if self.session.is_some() {
            return Err(DapError::SessionActive);
        }
        let session = SessionId::new();
        self.session = Some((session, lease_expires_at_ms));
        self.execute(session, kind.command(), arguments)
    }

    pub fn operate(
        &mut self,
        operation: DebugOperation,
        now_ms: u64,
    ) -> Result<DebugEvidence, DapError> {
        let (session, lease) = self.session.ok_or(DapError::NoSession)?;
        if now_ms >= lease {
            self.session = None;
            return Err(DapError::LeaseExpired);
        }
        let required = match operation {
            DebugOperation::ConfigurationDone(_) if !self.capabilities.configuration_done => {
                Some("configurationDone")
            }
            DebugOperation::Evaluate(_) if !self.capabilities.evaluate => Some("evaluate"),
            DebugOperation::ReadMemory(_) if !self.capabilities.read_memory => Some("readMemory"),
            DebugOperation::WriteMemory(_) if !self.capabilities.write_memory => {
                Some("writeMemory")
            }
            _ => None,
        };
        if let Some(command) = required {
            return Err(DapError::Unsupported(command));
        }
        self.execute(session, operation.command(), operation.arguments().clone())
    }

    /// What the adapter has said on its own since the last look.
    pub fn events(&mut self) -> Vec<Value> {
        self.transport.drain_events()
    }

    /// Wait for one of several events. See [`DapTransport::wait_for_event`].
    pub fn wait_for_event(
        &mut self,
        events: &[&str],
        deadline: Duration,
    ) -> Result<Option<Value>, DapError> {
        self.transport.wait_for_event(events, deadline)
    }

    pub const fn capabilities(&self) -> &DapCapabilities {
        &self.capabilities
    }

    pub fn terminate(&mut self) -> Result<DebugEvidence, DapError> {
        let (session, _) = self.session.take().ok_or(DapError::NoSession)?;
        if !self.capabilities.terminate {
            return Err(DapError::Unsupported("terminate"));
        }
        self.execute(session, "terminate", json!({}))
    }

    fn execute(
        &mut self,
        session: SessionId,
        command: &str,
        arguments: Value,
    ) -> Result<DebugEvidence, DapError> {
        if serde_json::to_vec(&arguments)
            .map_err(|error| DapError::Transport(error.to_string()))?
            .len()
            > MAX_DAP_EVIDENCE_BYTES
        {
            return Err(DapError::MessageTooLarge);
        }
        let response = self.transport.request(command, arguments.clone())?;
        let evidence = json!({
            "session": session,
            "request": {"command": command, "arguments": arguments},
            "response": response.clone(),
        });
        if serde_json::to_vec(&evidence)
            .map_err(|error| DapError::Transport(error.to_string()))?
            .len()
            > MAX_DAP_EVIDENCE_BYTES
        {
            return Err(DapError::MessageTooLarge);
        }
        let artifact = self.sink.capture(&evidence)?;
        Ok(DebugEvidence {
            session,
            artifact,
            command: command.into(),
            body: response,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum DapError {
    PolicyRequired(CapabilityAction),
    Unsupported(&'static str),
    SessionActive,
    NoSession,
    LeaseExpired,
    MessageTooLarge,
    Transport(String),
}

impl fmt::Display for DapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PolicyRequired(action) => write!(formatter, "debug operation requires {action}"),
            Self::Unsupported(command) => {
                write!(formatter, "debug adapter did not negotiate {command}")
            }
            Self::SessionActive => formatter.write_str("a debug session is already active"),
            Self::NoSession => formatter.write_str("no leased debug session is active"),
            Self::LeaseExpired => formatter.write_str("debug session lease expired"),
            Self::MessageTooLarge => {
                formatter.write_str("debug request or evidence exceeds its byte limit")
            }
            Self::Transport(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DapError {}

/// A debug adapter spoken to over its own stdin and stdout.
///
/// # The framing, and why it is not JSON lines
///
/// DAP frames every message with `Content-Length`, like LSP: a message may
/// contain newlines, so a line-oriented reader would split one message into
/// several and read the halves as garbage.
///
/// # Correlating a response with its request
///
/// Every request carries a sequence number and every response names the
/// request it answers. Events carry neither, and arrive whenever the debuggee
/// does something — so reading "the next message" would eventually return a
/// `stopped` event as though it were a response. Messages are read until one
/// answers *this* request; events found on the way are kept for
/// [`DapTransport::drain_events`].
pub struct StdioDapTransport {
    child: Child,
    channel: DapChannel<BufReader<ChildStdout>, ChildStdin>,
}

/// The protocol itself, over any pair of streams.
///
/// Separate from the process so the framing and the correlation can be tested
/// against a scripted adapter instead of a real one: those are where the bugs
/// are, and they have nothing to do with spawning.
pub struct DapChannel<R, W> {
    reader: R,
    writer: W,
    sequence: i64,
    events: Vec<Value>,
    deadline: Duration,
}

/// Longest single adapter message accepted. An adapter is a program the
/// operator configured, so this bounds a bug rather than an attack — but an
/// unbounded read here would let one take the process down.
pub const MAX_DAP_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

impl StdioDapTransport {
    /// Spawn an adapter. `argv` is the program and its arguments.
    pub fn spawn(argv: &[String], deadline: Duration) -> Result<Self, DapError> {
        let (program, arguments) = argv
            .split_first()
            .ok_or_else(|| DapError::Transport("a debug adapter needs a program".to_owned()))?;
        let mut child = Command::new(program)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The adapter's own diagnostics are its business; mixing them into
            // this process's stderr would interleave with ARSY's diagnostics.
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| DapError::Transport(format!("{program}: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| DapError::Transport("the adapter has no stdin".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DapError::Transport("the adapter has no stdout".to_owned()))?;
        Ok(Self {
            child,
            channel: DapChannel::new(BufReader::new(stdout), stdin, deadline),
        })
    }
}

impl<R: BufRead, W: Write> DapChannel<R, W> {
    pub const fn new(reader: R, writer: W, deadline: Duration) -> Self {
        Self {
            reader,
            writer,
            sequence: 0,
            events: Vec::new(),
            deadline,
        }
    }

    fn send(&mut self, message: &Value) -> Result<(), DapError> {
        let body = serde_json::to_string(message)
            .map_err(|error| DapError::Transport(error.to_string()))?;
        write!(self.writer, "Content-Length: {}\r\n\r\n{body}", body.len())
            .and_then(|()| self.writer.flush())
            .map_err(|error| DapError::Transport(error.to_string()))
    }

    /// Read one framed message.
    fn receive(&mut self) -> Result<Value, DapError> {
        let mut length = None;
        loop {
            let mut header = String::new();
            let read = self
                .reader
                .read_line(&mut header)
                .map_err(|error| DapError::Transport(error.to_string()))?;
            if read == 0 {
                return Err(DapError::Transport(
                    "the debug adapter closed its output".to_owned(),
                ));
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some(value) = header
                .strip_prefix("Content-Length:")
                .or_else(|| header.strip_prefix("content-length:"))
            {
                length = value.trim().parse::<usize>().ok();
            }
        }
        let length = length.ok_or_else(|| {
            DapError::Transport("an adapter message carried no Content-Length".to_owned())
        })?;
        if length > MAX_DAP_MESSAGE_BYTES {
            return Err(DapError::MessageTooLarge);
        }
        let mut body = vec![0; length];
        self.reader
            .read_exact(&mut body)
            .map_err(|error| DapError::Transport(error.to_string()))?;
        serde_json::from_slice(&body).map_err(|error| DapError::Transport(error.to_string()))
    }
}

impl<R: BufRead, W: Write> DapTransport for DapChannel<R, W> {
    fn initialize(&mut self) -> Result<DapCapabilities, DapError> {
        let body = self.request(
            "initialize",
            json!({
                "clientID": "arsy",
                "adapterID": "arsy",
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "pathFormat": "path",
            }),
        )?;
        let flag = |name: &str| body.get(name).and_then(Value::as_bool).unwrap_or(false);
        Ok(DapCapabilities {
            // `attach` and `evaluate` are requests every adapter is expected
            // to answer: there is no flag that advertises them, and one that
            // cannot fails the request rather than declining it up front.
            attach: true,
            evaluate: true,
            configuration_done: flag("supportsConfigurationDoneRequest"),
            read_memory: flag("supportsReadMemoryRequest"),
            write_memory: flag("supportsWriteMemoryRequest"),
            terminate: flag("supportsTerminateRequest"),
        })
    }

    fn request(&mut self, command: &str, arguments: Value) -> Result<Value, DapError> {
        self.sequence += 1;
        let sequence = self.sequence;
        self.send(&json!({
            "seq": sequence,
            "type": "request",
            "command": command,
            "arguments": arguments,
        }))?;
        let started = Instant::now();
        loop {
            if started.elapsed() > self.deadline {
                return Err(DapError::Transport(format!(
                    "the debug adapter did not answer {command} within {:?}",
                    self.deadline
                )));
            }
            let message = self.receive()?;
            match message.get("type").and_then(Value::as_str) {
                Some("response") if message.get("request_seq") == Some(&json!(sequence)) => {
                    if message.get("success").and_then(Value::as_bool) == Some(false) {
                        return Err(DapError::Transport(format!(
                            "{command} failed: {}",
                            message
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("the adapter gave no reason")
                        )));
                    }
                    return Ok(message.get("body").cloned().unwrap_or(Value::Null));
                }
                Some("event") => self.events.push(message),
                // A response to something else, or a reverse request this
                // client does not implement: neither is this call's answer.
                _ => {}
            }
        }
    }

    fn drain_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.events)
    }

    fn wait_for_event(
        &mut self,
        events: &[&str],
        deadline: Duration,
    ) -> Result<Option<Value>, DapError> {
        let named = |message: &Value| {
            message
                .get("event")
                .and_then(Value::as_str)
                .is_some_and(|event| events.contains(&event))
        };
        if let Some(index) = self.events.iter().position(named) {
            return Ok(Some(self.events.remove(index)));
        }
        let started = Instant::now();
        while started.elapsed() < deadline {
            let message = match self.receive() {
                Ok(message) => message,
                // The adapter closing its output is the answer: nothing more
                // is coming, and that is not an error in a wait.
                Err(DapError::Transport(_)) => return Ok(None),
                Err(error) => return Err(error),
            };
            if message.get("type").and_then(Value::as_str) != Some("event") {
                continue;
            }
            if named(&message) {
                return Ok(Some(message));
            }
            self.events.push(message);
        }
        Ok(None)
    }
}

impl DapTransport for StdioDapTransport {
    fn initialize(&mut self) -> Result<DapCapabilities, DapError> {
        self.channel.initialize()
    }

    fn request(&mut self, command: &str, arguments: Value) -> Result<Value, DapError> {
        self.channel.request(command, arguments)
    }

    fn drain_events(&mut self) -> Vec<Value> {
        self.channel.drain_events()
    }

    fn wait_for_event(
        &mut self,
        events: &[&str],
        deadline: Duration,
    ) -> Result<Option<Value>, DapError> {
        self.channel.wait_for_event(events, deadline)
    }
}

impl Drop for StdioDapTransport {
    /// An adapter left running would outlive the operation that started it.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Transport;

    impl DapTransport for Transport {
        fn initialize(&mut self) -> Result<DapCapabilities, DapError> {
            Ok(DapCapabilities {
                attach: false,
                evaluate: true,
                terminate: true,
                ..DapCapabilities::default()
            })
        }

        fn request(&mut self, command: &str, _arguments: Value) -> Result<Value, DapError> {
            Ok(json!({"command": command, "success": true}))
        }
    }

    struct Sink(usize);

    impl DebugEvidenceSink for Sink {
        fn capture(&mut self, _value: &Value) -> Result<ResourceRef, DapError> {
            self.0 += 1;
            ResourceRef::new("artifact", format!("debug/{}", self.0))
                .map_err(|error| DapError::Transport(error.to_string()))
        }
    }

    #[test]
    fn capabilities_are_runtime_facts_and_every_operation_cites_an_artifact() {
        let mut host = DapHost::connect(Transport, Sink(0)).unwrap();
        assert_eq!(
            host.start(
                DebugStart::Attach,
                json!({}),
                &BTreeSet::from([CapabilityAction::DebugAttach]),
                10
            ),
            Err(DapError::Unsupported("attach"))
        );
        assert!(matches!(
            host.start(DebugStart::Launch, json!({}), &BTreeSet::new(), 10),
            Err(DapError::PolicyRequired(CapabilityAction::DebugLaunch))
        ));
        let evidence = host
            .start(
                DebugStart::Launch,
                json!({"program": "app"}),
                &BTreeSet::from([CapabilityAction::DebugLaunch]),
                10,
            )
            .unwrap();
        assert_eq!(evidence.artifact.scheme(), "artifact");
        assert!(matches!(
            host.operate(DebugOperation::ReadMemory(json!({})), 1),
            Err(DapError::Unsupported("readMemory"))
        ));
        assert_eq!(host.terminate().unwrap().command, "terminate");
        assert!(matches!(host.terminate(), Err(DapError::NoSession)));
    }

    /// Frame a message the way an adapter does.
    fn framed(message: Value) -> String {
        let body = message.to_string();
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    #[test]
    fn a_response_is_matched_by_sequence_and_events_are_kept_not_mistaken_for_one() {
        let script = format!(
            "{}{}{}",
            // An event arrives before the answer, which is the ordinary case.
            framed(json!({"type": "event", "event": "output", "body": {"output": "starting\n"}})),
            // A response to something else must not be read as this answer.
            framed(
                json!({"type": "response", "request_seq": 99, "success": true, "body": {"wrong": true}})
            ),
            framed(
                json!({"type": "response", "request_seq": 1, "success": true, "body": {"stackFrames": []}})
            ),
        );
        let mut channel = DapChannel::new(script.as_bytes(), Vec::new(), Duration::from_secs(5));

        let body = channel
            .request("stackTrace", json!({"threadId": 1}))
            .unwrap();

        assert_eq!(body, json!({"stackFrames": []}));
        let events = channel.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "output");
        assert!(
            channel.drain_events().is_empty(),
            "an event is reported once"
        );
    }

    #[test]
    fn the_request_is_framed_with_its_length_and_a_sequence_that_advances() {
        let script = format!(
            "{}{}",
            framed(json!({"type": "response", "request_seq": 1, "success": true, "body": {}})),
            framed(json!({"type": "response", "request_seq": 2, "success": true, "body": {}})),
        );
        let mut channel = DapChannel::new(script.as_bytes(), Vec::new(), Duration::from_secs(5));

        channel.request("first", json!({})).unwrap();
        channel.request("second", json!({})).unwrap();

        let sent = String::from_utf8(std::mem::take(&mut channel.writer)).unwrap();
        assert!(sent.starts_with("Content-Length: "), "{sent}");
        assert!(sent.contains("\"seq\":1"), "{sent}");
        assert!(sent.contains("\"seq\":2"), "{sent}");
        // The header and the body are separated by a blank line, and the
        // declared length is the body's.
        for frame in sent.split("Content-Length: ").skip(1) {
            let (length, body) = frame.split_once("\r\n\r\n").expect("a framed message");
            assert_eq!(length.trim().parse::<usize>().unwrap(), body.len());
        }
    }

    #[test]
    fn an_adapter_that_refuses_a_request_says_so_rather_than_returning_nothing() {
        let script = framed(
            json!({"type": "response", "request_seq": 1, "success": false, "message": "no such breakpoint"}),
        );
        let mut channel = DapChannel::new(script.as_bytes(), Vec::new(), Duration::from_secs(5));

        let error = channel
            .request("setBreakpoints", json!({}))
            .expect_err("a failed response is an error");

        assert!(format!("{error}").contains("no such breakpoint"), "{error}");
    }

    #[test]
    fn capabilities_come_from_the_flags_the_protocol_defines() {
        let script = framed(json!({
            "type": "response",
            "request_seq": 1,
            "success": true,
            "body": {"supportsConfigurationDoneRequest": true, "supportsTerminateRequest": false},
        }));
        let mut channel = DapChannel::new(script.as_bytes(), Vec::new(), Duration::from_secs(5));

        let capabilities = channel.initialize().unwrap();

        assert!(capabilities.configuration_done);
        assert!(!capabilities.terminate, "an absent flag is not a promise");
        assert!(!capabilities.read_memory);
        // Requests with no flag of their own are not declined up front.
        assert!(capabilities.evaluate);
        assert!(capabilities.attach);
    }

    #[test]
    fn an_adapter_that_stops_talking_mid_message_is_an_error_not_a_hang() {
        let mut channel = DapChannel::new(
            "Content-Length: 40\r\n\r\n{\"type\"".as_bytes(),
            Vec::new(),
            Duration::from_secs(5),
        );
        assert!(channel.request("initialize", json!({})).is_err());
    }
}
