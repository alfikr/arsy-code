use arsy_kernel::{
    capability::CapabilityAction,
    domain::{ResourceRef, SessionId},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeSet, fmt};

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
}

pub trait DapTransport {
    fn initialize(&mut self) -> Result<DapCapabilities, DapError>;
    fn request(&mut self, command: &str, arguments: Value) -> Result<Value, DapError>;
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
            "response": response,
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
}
