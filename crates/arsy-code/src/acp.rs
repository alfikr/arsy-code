//! The ACP adapter: an editor's session vocabulary, mapped onto ARSY's.
//!
//! See `docs/24-mcp-acp.md`. ACP is an edge protocol, so its request types stop
//! here: every method becomes a `ClientRequest` the canonical service already
//! understands, and every server event becomes an ACP update. Nothing in this
//! module decides authority — it translates, and the service decides.
//!
//! Two conversions are the adapter's own responsibility, because they are where
//! the two vocabularies genuinely differ:
//!
//! * **Absolute paths and 1-based lines.** ACP speaks the editor's coordinates.
//!   They are canonicalized on the way in and restored on the way out, so no
//!   layer inside ARSY ever has to know which convention a caller used.
//! * **`_meta` and underscore methods carry no authority.** They are preserved
//!   as extension data and never consulted when deciding anything.

use arsy_kernel::{
    domain::{AgentId, ResourceRef, SessionId, WorkspaceId},
    protocol::{
        AgentAction, AgentControl, ClientRequest, Extensions, ProtocolVersion, ServerEvent,
        SessionCreate, SessionResume, TurnStart,
    },
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// ACP revision this adapter implements.
pub const ACP_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcpError {
    /// The method is not one this build implements.
    UnknownMethod(String),
    /// The parameters are missing something the method needs.
    InvalidParams(String),
    /// A path that is not inside the workspace the session opened.
    OutsideWorkspace(PathBuf),
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownMethod(method) => write!(formatter, "`{method}` is not implemented"),
            Self::InvalidParams(detail) => formatter.write_str(detail),
            Self::OutsideWorkspace(path) => {
                write!(formatter, "{} is outside the workspace", path.display())
            }
        }
    }
}

impl std::error::Error for AcpError {}

/// What one ACP request becomes.
#[derive(Clone, Debug, PartialEq)]
pub enum Translated {
    /// A canonical request to hand to the service.
    Request(ClientRequest),
    /// An answer the adapter can give without the service: negotiation, and
    /// the capability advertisement that goes with it.
    Answer(Value),
}

/// One editor session's adapter.
pub struct AcpAdapter {
    workspace_root: PathBuf,
    workspace: WorkspaceId,
}

impl AcpAdapter {
    pub fn new(workspace_root: impl AsRef<Path>, workspace: WorkspaceId) -> Self {
        Self {
            workspace_root: workspace_root.as_ref().to_path_buf(),
            workspace,
        }
    }

    /// Map one ACP method and its parameters.
    pub fn request(&self, method: &str, params: &Value) -> Result<Translated, AcpError> {
        // An underscore method is an extension. It is answered as a no-op
        // rather than routed, so an extension can never reach core authority.
        if method.starts_with('_') {
            return Ok(Translated::Answer(json!({"_meta": {"ignored": method}})));
        }
        match method {
            "initialize" => Ok(Translated::Answer(self.initialize())),
            "authenticate" => Ok(Translated::Answer(json!({
                // Credentials belong to `arsy auth`, which runs where the
                // operator is. An editor is never asked for one.
                "authenticated": true,
                "methods": [],
            }))),
            "session/new" => Ok(Translated::Request(ClientRequest::SessionCreate(
                SessionCreate {
                    workspace: self.workspace,
                    extensions: extensions(params),
                },
            ))),
            "session/load" => Ok(Translated::Request(ClientRequest::SessionResume(
                SessionResume {
                    session: self.session(params)?,
                    from_sequence: params.get("fromSequence").and_then(Value::as_u64),
                    extensions: extensions(params),
                },
            ))),
            "session/prompt" => Ok(Translated::Request(ClientRequest::TurnStart(TurnStart {
                session: self.session(params)?,
                prompt: prompt_text(params)?,
                extensions: extensions(params),
            }))),
            "session/cancel" => Ok(Translated::Request(ClientRequest::AgentControl(
                AgentControl {
                    agent: agent(params)?,
                    action: AgentAction::Interrupt,
                    extensions: extensions(params),
                },
            ))),
            other => Err(AcpError::UnknownMethod(other.to_owned())),
        }
    }

    fn initialize(&self) -> Value {
        let ProtocolVersion { major, minor } = ProtocolVersion::CURRENT;
        json!({
            "protocolVersion": ACP_VERSION,
            // Only implemented capabilities are advertised. Filesystem and
            // terminal methods are absent because this build does not serve
            // them over ACP, and claiming them would strand a client.
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false},
            },
            "authMethods": [],
            "_meta": {"arsyProtocol": format!("{major}.{minor}")},
        })
    }

    fn session(&self, params: &Value) -> Result<SessionId, AcpError> {
        params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| AcpError::InvalidParams("a sessionId is required".to_owned()))?
            .parse()
            .map_err(|_| AcpError::InvalidParams("sessionId is not a canonical ID".to_owned()))
    }

    /// An absolute path from the editor as a workspace-relative resource.
    ///
    /// Rejecting a path outside the workspace here means no later layer has to
    /// wonder whether an editor-supplied path was checked.
    pub fn resource(&self, path: &Path) -> Result<ResourceRef, AcpError> {
        let relative = path
            .strip_prefix(&self.workspace_root)
            .map_err(|_| AcpError::OutsideWorkspace(path.to_owned()))?;
        if relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(AcpError::OutsideWorkspace(path.to_owned()));
        }
        ResourceRef::new(
            "file",
            relative
                .components()
                .filter_map(|component| component.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join("/"),
        )
        .map_err(|error| AcpError::InvalidParams(error.to_string()))
    }

    /// The absolute path an editor expects for a workspace-relative resource.
    pub fn absolute(&self, resource: &ResourceRef) -> PathBuf {
        self.workspace_root.join(resource.value())
    }
}

/// ACP counts lines from one; everything inside ARSY counts from zero.
pub const fn line_to_internal(one_based: u32) -> u32 {
    one_based.saturating_sub(1)
}

pub const fn line_to_acp(zero_based: u32) -> u32 {
    zero_based.saturating_add(1)
}

/// A prompt as ACP sends it: a list of content blocks, of which this build
/// reads the text ones.
fn prompt_text(params: &Value) -> Result<String, AcpError> {
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| AcpError::InvalidParams("a prompt array is required".to_owned()))?;
    let text: Vec<&str> = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    if text.is_empty() {
        // Silently sending an empty prompt would look like the model ignored
        // an image the client did send.
        return Err(AcpError::InvalidParams(
            "the prompt carries no text block; this build advertises no other content type"
                .to_owned(),
        ));
    }
    Ok(text.join("\n"))
}

fn agent(params: &Value) -> Result<AgentId, AcpError> {
    match params.get("agentId").and_then(Value::as_str) {
        // Cancelling a session with no agent named cancels its own agent,
        // which the service resolves; a fresh id would name nothing.
        None => Ok(AgentId::new()),
        Some(value) => value
            .parse()
            .map_err(|_| AcpError::InvalidParams("agentId is not a canonical ID".to_owned())),
    }
}

/// `_meta` preserved as extension data. It travels with the request and decides
/// nothing.
fn extensions(params: &Value) -> Extensions {
    let mut extensions = Extensions::new();
    if let Some(meta) = params.get("_meta") {
        extensions.insert("_meta".to_owned(), meta.clone());
    }
    extensions
}

/// One canonical server event as an ACP session update.
///
/// Events ACP has no shape for are `None` rather than an invented update: a
/// client that receives an update it cannot interpret is worse off than one
/// that receives nothing.
pub fn update(event: &ServerEvent) -> Option<Value> {
    match event {
        ServerEvent::Subscribed {
            subscription,
            next_sequence,
        } => Some(json!({
            "sessionUpdate": "subscribed",
            "_meta": {
                "subscription": subscription.to_string(),
                "nextSequence": next_sequence,
            },
        })),
        ServerEvent::Stream { envelope, .. } => Some(json!({
            "sessionUpdate": match envelope.kind.as_str() {
                "turn.started" => "agent_message_chunk",
                "turn.completed" => "agent_turn_completed",
                "turn.failed" => "agent_turn_failed",
                _ => "agent_thought_chunk",
            },
            "sessionId": envelope.session.to_string(),
            "_meta": {
                "kind": envelope.kind,
                "sequence": envelope.sequence,
                "eventId": envelope.id.to_string(),
            },
        })),
        ServerEvent::Gap {
            dropped_from,
            next_sequence,
            ..
        } => Some(json!({
            "sessionUpdate": "gap",
            "_meta": {"droppedFrom": dropped_from, "nextSequence": next_sequence},
        })),
        // A failure is answered on the request that caused it, not as an
        // unsolicited session update.
        ServerEvent::Failed { .. } => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> AcpAdapter {
        AcpAdapter::new("/work/repo", WorkspaceId::new())
    }

    #[test]
    fn session_methods_map_onto_canonical_requests() {
        let adapter = adapter();
        let session = SessionId::new();

        assert!(matches!(
            adapter.request("session/new", &json!({})).unwrap(),
            Translated::Request(ClientRequest::SessionCreate(_))
        ));

        let Translated::Request(ClientRequest::SessionResume(resumed)) = adapter
            .request(
                "session/load",
                &json!({"sessionId": session.to_string(), "fromSequence": 12}),
            )
            .unwrap()
        else {
            panic!("session/load resumes");
        };
        assert_eq!(resumed.session, session);
        assert_eq!(resumed.from_sequence, Some(12));

        let Translated::Request(ClientRequest::TurnStart(turn)) = adapter
            .request(
                "session/prompt",
                &json!({
                    "sessionId": session.to_string(),
                    "prompt": [
                        {"type": "text", "text": "explain this"},
                        {"type": "image", "data": "..."},
                        {"type": "text", "text": "and that"},
                    ],
                }),
            )
            .unwrap()
        else {
            panic!("session/prompt starts a turn");
        };
        assert_eq!(turn.prompt, "explain this\nand that");

        let Translated::Request(ClientRequest::AgentControl(control)) = adapter
            .request("session/cancel", &json!({"sessionId": session.to_string()}))
            .unwrap()
        else {
            panic!("session/cancel interrupts");
        };
        assert_eq!(control.action, AgentAction::Interrupt);
    }

    #[test]
    fn negotiation_advertises_only_implemented_capabilities() {
        let Translated::Answer(answer) = adapter().request("initialize", &json!({})).unwrap()
        else {
            panic!("initialize is answered by the adapter");
        };
        assert_eq!(answer["protocolVersion"], ACP_VERSION);
        assert_eq!(answer["agentCapabilities"]["loadSession"], true);
        assert_eq!(
            answer["agentCapabilities"]["promptCapabilities"]["image"],
            false
        );
        let capabilities = answer["agentCapabilities"].as_object().unwrap();
        for unimplemented in ["fs", "terminal"] {
            assert!(!capabilities.contains_key(unimplemented));
        }

        assert_eq!(
            adapter().request("terminal/create", &json!({})),
            Err(AcpError::UnknownMethod("terminal/create".to_owned()))
        );
    }

    #[test]
    fn an_underscore_method_and_meta_carry_no_authority() {
        let adapter = adapter();
        let Translated::Answer(answer) =
            adapter.request("_zed/awaitCompletion", &json!({})).unwrap()
        else {
            panic!("an extension method is answered, not routed");
        };
        assert_eq!(answer["_meta"]["ignored"], "_zed/awaitCompletion");

        // `_meta` on a real method travels with it and is not interpreted.
        let Translated::Request(ClientRequest::SessionCreate(created)) = adapter
            .request(
                "session/new",
                &json!({"_meta": {"trusted": true, "capabilities": ["fs.write:**"]}}),
            )
            .unwrap()
        else {
            panic!("session/new creates");
        };
        assert_eq!(created.extensions.get("_meta").unwrap()["trusted"], true);
        assert_eq!(
            created.workspace, adapter.workspace,
            "the workspace comes from the adapter, never from the client's `_meta`"
        );
    }

    #[test]
    fn editor_coordinates_are_converted_at_the_boundary() {
        let adapter = adapter();
        let resource = adapter
            .resource(Path::new("/work/repo/src/main.rs"))
            .unwrap();
        assert_eq!(resource.scheme(), "file");
        assert_eq!(resource.value(), "src/main.rs");
        assert_eq!(
            adapter.absolute(&resource),
            PathBuf::from("/work/repo/src/main.rs")
        );

        for outside in ["/etc/passwd", "/work/other/file.rs"] {
            assert!(
                matches!(
                    adapter.resource(Path::new(outside)),
                    Err(AcpError::OutsideWorkspace(_))
                ),
                "{outside}"
            );
        }

        assert_eq!(line_to_internal(1), 0);
        assert_eq!(line_to_internal(0), 0, "a malformed zero does not wrap");
        assert_eq!(line_to_acp(0), 1);
        assert_eq!(line_to_acp(line_to_internal(42)), 42);
    }

    #[test]
    fn a_prompt_with_no_text_is_refused_rather_than_sent_empty() {
        let adapter = adapter();
        for refused in [
            json!({"sessionId": SessionId::new().to_string(), "prompt": []}),
            json!({"sessionId": SessionId::new().to_string(),
                   "prompt": [{"type": "image", "data": "..."}]}),
            json!({"sessionId": SessionId::new().to_string()}),
            json!({"prompt": [{"type": "text", "text": "hi"}]}),
        ] {
            assert!(
                matches!(
                    adapter.request("session/prompt", &refused),
                    Err(AcpError::InvalidParams(_))
                ),
                "{refused}"
            );
        }
    }
}
