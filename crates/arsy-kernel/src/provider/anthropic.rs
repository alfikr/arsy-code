//! Anthropic Messages adapter: wire format, authentication, error normalization.
//!
//! HTTP itself is injected as [`WireTransport`] rather than depended on, so the
//! part that differs per provider — body shape, headers, status mapping, SSE
//! semantics — is the part that is testable here.

pub use super::wire::{ApiKey, WireRequest, WireResponse, WireTransport};
use super::{
    CanonicalModelRequest, ModelContent, ModelEvent, ModelEventStream, ModelProvider, ModelRole,
    ProviderDescriptor, ProviderError, StopReason,
};
use crate::secret::Redactor;
use serde_json::{json, Map, Value};
use std::collections::VecDeque;

/// Wire version Anthropic requires on every request.
pub const API_VERSION: &str = "2023-06-01";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

pub struct AnthropicProvider<T> {
    descriptor: ProviderDescriptor,
    base_url: String,
    key: ApiKey,
    transport: T,
    redactor: Redactor,
}

impl<T: WireTransport> AnthropicProvider<T> {
    pub fn new(key: ApiKey, transport: T) -> Self {
        Self::with_base_url(DEFAULT_BASE_URL, key, transport)
    }

    pub fn with_base_url(base_url: impl Into<String>, key: ApiKey, transport: T) -> Self {
        Self {
            descriptor: ProviderDescriptor {
                id: "anthropic".to_owned(),
                max_retries: 3,
            },
            base_url: base_url.into(),
            key,
            transport,
            redactor: Redactor::new(),
        }
    }

    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Canonical request to Anthropic Messages wire form.
    pub fn encode(&self, request: &CanonicalModelRequest) -> WireRequest {
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(request.model.model));
        body.insert("max_tokens".to_owned(), json!(request.max_output_tokens));
        body.insert("stream".to_owned(), json!(true));
        if let Some(system) = &request.system {
            body.insert("system".to_owned(), json!(system));
        }
        body.insert(
            "messages".to_owned(),
            Value::Array(request.messages.iter().map(encode_message).collect()),
        );
        if !request.tools.is_empty() {
            body.insert(
                "tools".to_owned(),
                Value::Array(
                    request
                        .tools
                        .iter()
                        .map(|tool| {
                            json!({
                                "name": tool.name,
                                "description": tool.description,
                                "input_schema": tool.input_schema,
                            })
                        })
                        .collect(),
                ),
            );
        }
        WireRequest {
            url: format!("{}/v1/messages", self.base_url.trim_end_matches('/')),
            headers: vec![
                ("x-api-key".to_owned(), self.key.expose().to_owned()),
                ("anthropic-version".to_owned(), API_VERSION.to_owned()),
                ("content-type".to_owned(), "application/json".to_owned()),
                ("accept".to_owned(), "text/event-stream".to_owned()),
            ],
            body: Value::Object(body).to_string(),
        }
    }
}

fn encode_message(message: &super::ModelMessage) -> Value {
    json!({
        "role": match message.role {
            ModelRole::User => "user",
            ModelRole::Assistant => "assistant",
        },
        "content": message.content.iter().map(encode_content).collect::<Vec<_>>(),
    })
}

fn encode_content(content: &ModelContent) -> Value {
    match content {
        ModelContent::Text { text } => json!({ "type": "text", "text": text }),
        ModelContent::ToolCall {
            id,
            name,
            arguments,
        } => json!({ "type": "tool_use", "id": id, "name": name, "input": arguments }),
        ModelContent::ToolResult {
            id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": content,
            "is_error": is_error,
        }),
    }
}

impl<T: WireTransport> ModelProvider for AnthropicProvider<T> {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn stream(&self, request: &CanonicalModelRequest) -> Result<ModelEventStream, ProviderError> {
        let mut wire = self.encode(request);
        wire.body = self
            .redactor
            .sanitize(&wire.body)
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
        let response = self.transport.send(wire)?;
        if response.status != 200 {
            return Err(normalize_status(response));
        }
        Ok(Box::new(EventDecoder::new(response.lines)))
    }
}

/// HTTP failure to normalized error, including the provider's retry hint.
///
/// A non-200 response carries a JSON error body rather than an event stream, so
/// the same line iterator is drained as the body.
fn normalize_status(response: WireResponse) -> ProviderError {
    let status = response.status;
    let retry_after = response.retry_after();
    let body: String = response.lines.filter_map(Result::ok).collect();
    let message = error_message(&body).unwrap_or_else(|| format!("http {status}"));
    match status {
        401 | 403 => ProviderError::Auth(message),
        400 | 413 | 422 => ProviderError::InvalidRequest(message),
        404 => ProviderError::NotFound(message),
        429 => ProviderError::RateLimited { retry_after },
        status => ProviderError::Server { status, message },
    }
}

fn error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    let kind = error.get("type").and_then(Value::as_str).unwrap_or("error");
    let detail = error.get("message").and_then(Value::as_str)?;
    Some(format!("{kind}: {detail}"))
}

/// Server-sent-event decoder for the Messages stream.
///
/// Tool arguments arrive as `input_json_delta` fragments that are only valid
/// JSON once concatenated. Fragments are surfaced as
/// [`ModelEvent::ToolCallDelta`] and buffered; the call becomes executable only
/// at `content_block_stop`, when the accumulated text parses. A stream that is
/// cut short therefore yields no executable call.
struct EventDecoder {
    lines: Box<dyn Iterator<Item = Result<String, String>> + Send>,
    blocks: Vec<ToolBlock>,
    /// One wire event can carry two canonical facts (usage and stop reason).
    queue: VecDeque<ModelEvent>,
    done: bool,
}

struct ToolBlock {
    id: String,
    name: String,
    arguments: String,
}

impl EventDecoder {
    fn new(lines: Box<dyn Iterator<Item = Result<String, String>> + Send>) -> Self {
        Self {
            lines,
            blocks: Vec::new(),
            queue: VecDeque::new(),
            done: false,
        }
    }

    /// Decode one `data:` payload into zero or more canonical events.
    fn decode(&mut self, payload: &str) -> Result<(), ProviderError> {
        let value: Value = serde_json::from_str(payload)
            .map_err(|error| ProviderError::Decode(error.to_string()))?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::Decode("event is missing `type`".to_owned()))?;
        match kind {
            "content_block_start" => self.block_start(&value),
            "content_block_delta" => self.block_delta(&value),
            "content_block_stop" => self.block_stop(&value),
            "message_delta" => {
                // One wire event reports usage and the stop reason together.
                if let Some(usage) = value.get("usage") {
                    self.queue.push_back(ModelEvent::Usage {
                        input_tokens: count(usage, "input_tokens"),
                        output_tokens: count(usage, "output_tokens"),
                    });
                }
                if let Some(stop) = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                    .map(stop_reason)
                {
                    self.queue.push_back(ModelEvent::Completed { stop });
                }
                Ok(())
            }
            "message_stop" => {
                self.done = true;
                Ok(())
            }
            "error" => Err(normalize_stream_error(&value)),
            // `message_start`, `ping`, and anything added later.
            _ => Ok(()),
        }
    }

    /// Only `tool_use` blocks are tracked; text needs no per-block state.
    fn block_start(&mut self, value: &Value) -> Result<(), ProviderError> {
        let index = block_index(value)?;
        let block = value
            .get("content_block")
            .ok_or_else(|| ProviderError::Decode("missing `content_block`".to_owned()))?;
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            return Ok(());
        }
        let id = field(block, "id")?.to_owned();
        let name = field(block, "name")?.to_owned();
        if self.blocks.len() != index {
            return Err(ProviderError::Decode(format!(
                "content block {index} started out of order"
            )));
        }
        self.blocks.push(ToolBlock {
            id: id.clone(),
            name: name.clone(),
            arguments: String::new(),
        });
        self.queue
            .push_back(ModelEvent::ToolCallStarted { index, id, name });
        Ok(())
    }

    fn block_delta(&mut self, value: &Value) -> Result<(), ProviderError> {
        let index = block_index(value)?;
        let delta = value
            .get("delta")
            .ok_or_else(|| ProviderError::Decode("missing `delta`".to_owned()))?;
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => self.queue.push_back(ModelEvent::TextDelta {
                text: field(delta, "text")?.to_owned(),
            }),
            Some("input_json_delta") => {
                let fragment = field(delta, "partial_json")?.to_owned();
                let block = self.blocks.get_mut(index).ok_or_else(|| {
                    ProviderError::Decode(format!("delta for unstarted block {index}"))
                })?;
                block.arguments.push_str(&fragment);
                self.queue
                    .push_back(ModelEvent::ToolCallDelta { index, fragment });
            }
            // Thinking and signature deltas carry nothing canonical yet.
            _ => {}
        }
        Ok(())
    }

    /// The only place a tool call becomes executable: the buffered fragments
    /// must parse as a whole, or the call is rejected rather than guessed at.
    fn block_stop(&mut self, value: &Value) -> Result<(), ProviderError> {
        let index = block_index(value)?;
        let Some(block) = self.blocks.get(index) else {
            return Ok(());
        };
        // An empty-input tool call is legal and encodes as `{}`.
        let raw = if block.arguments.trim().is_empty() {
            "{}"
        } else {
            &block.arguments
        };
        let arguments = serde_json::from_str(raw).map_err(|error| {
            ProviderError::Decode(format!(
                "tool arguments for block {index} never completed: {error}"
            ))
        })?;
        self.queue.push_back(ModelEvent::ToolCallCompleted {
            index,
            id: block.id.clone(),
            name: block.name.clone(),
            arguments,
        });
        Ok(())
    }
}

impl Iterator for EventDecoder {
    type Item = Result<ModelEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Some(Ok(event));
            }
            if self.done {
                return None;
            }
            let line = match self.lines.next()? {
                Ok(line) => line,
                Err(error) => {
                    self.done = true;
                    return Some(Err(ProviderError::Transport(error)));
                }
            };
            let Some(payload) = line.trim_end().strip_prefix("data:") else {
                continue;
            };
            if let Err(error) = self.decode(payload.trim()) {
                self.done = true;
                self.queue.clear();
                return Some(Err(error));
            }
        }
    }
}

fn normalize_stream_error(value: &Value) -> ProviderError {
    let error = value.get("error");
    let kind = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("stream error")
        .to_owned();
    match kind {
        "authentication_error" | "permission_error" => ProviderError::Auth(message),
        "invalid_request_error" => ProviderError::InvalidRequest(message),
        "not_found_error" => ProviderError::NotFound(message),
        "rate_limit_error" => ProviderError::RateLimited { retry_after: None },
        "overloaded_error" => ProviderError::Server {
            status: 529,
            message,
        },
        _ => ProviderError::Server {
            status: 500,
            message,
        },
    }
}

fn stop_reason(raw: &str) -> StopReason {
    match raw {
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        _ => StopReason::Other,
    }
}

fn block_index(value: &Value) -> Result<usize, ProviderError> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .map(|index| index as usize)
        .ok_or_else(|| ProviderError::Decode("event is missing `index`".to_owned()))
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, ProviderError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::Decode(format!("event is missing `{name}`")))
}

fn count(usage: &Value, name: &str) -> u64 {
    usage.get(name).and_then(Value::as_u64).unwrap_or_default()
}
