//! stdio JSON-lines transport: one JSON object per line, in both directions.
//!
//! The first transport for the canonical protocol; see `docs/20-protocols.md`.
//! It owns framing and bounds only — every semantic decision stays with the
//! handler, so the whole request surface passes through unchanged.

use crate::{
    domain::RequestId,
    protocol::{decode_request, ClientRequest, ProtocolEnvelope, ProtocolError, ServerEvent},
    secret::{Redactor, SecretError},
};
use std::{
    fmt,
    io::{self, BufRead, Read, Write},
};
use uuid::Uuid;

/// Largest accepted line, including its newline. Above the maximum inline event
/// payload with room for JSON escaping, so a legal event always fits.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Framing for the canonical protocol over a byte stream.
///
/// Parse depth is bounded by `serde_json`'s recursion limit, which surfaces as
/// [`ProtocolError::Malformed`] rather than a stack overflow.
pub struct StdioTransport<R, W> {
    reader: R,
    writer: W,
    redactor: Redactor,
}

impl<R: BufRead, W: Write> StdioTransport<R, W> {
    pub const fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            redactor: Redactor::new(),
        }
    }

    /// Same transport with the broker's redaction pipeline installed. This is a
    /// protocol adapter, so everything written passes through it first.
    pub fn with_redactor(reader: R, writer: W, redactor: Redactor) -> Self {
        Self {
            reader,
            writer,
            redactor,
        }
    }

    /// Read one request; `Ok(None)` at end of input.
    pub fn read_request(
        &mut self,
    ) -> Result<Option<ProtocolEnvelope<ClientRequest>>, TransportError> {
        match self.read_message()? {
            Some(line) => Ok(Some(decode_request(&line)?)),
            None => Ok(None),
        }
    }

    /// Write one server event as a single line, flushed so an embedding client
    /// sees it without waiting for the next one.
    ///
    /// Redaction runs on the serialized line and fails closed: a line that
    /// still carries a credential is never written.
    pub fn write_event(&mut self, event: ServerEvent) -> Result<(), TransportError> {
        let line = serde_json::to_string(&ProtocolEnvelope::new(event))
            .map_err(|error| ProtocolError::Malformed(error.to_string()))?;
        let line = self.redactor.sanitize(&line)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }

    /// Serve until end of input. A rejected message is reported as `failed` and
    /// the stream continues; only an I/O failure ends the loop.
    pub fn serve(
        &mut self,
        mut handler: impl FnMut(ProtocolEnvelope<ClientRequest>) -> Vec<ServerEvent>,
    ) -> Result<(), TransportError> {
        loop {
            let line = match self.read_message() {
                Ok(Some(line)) => line,
                Ok(None) => return Ok(()),
                Err(TransportError::Io(error)) => return Err(TransportError::Io(error)),
                Err(error) => {
                    self.write_event(failure(uncorrelated(), &error))?;
                    continue;
                }
            };
            match decode_request(&line) {
                Ok(request) => {
                    for event in handler(request) {
                        self.write_event(event)?;
                    }
                }
                Err(error) => {
                    let request_id = recover_request_id(&line);
                    self.write_event(failure(request_id, &error.into()))?;
                }
            }
        }
    }

    /// Read one non-empty line, rejecting anything over [`MAX_MESSAGE_BYTES`].
    fn read_message(&mut self) -> Result<Option<String>, TransportError> {
        loop {
            let mut buffer = Vec::new();
            let read = Read::take(&mut self.reader, MAX_MESSAGE_BYTES as u64 + 1)
                .read_until(b'\n', &mut buffer)?;
            if read == 0 {
                return Ok(None);
            }
            if buffer.len() > MAX_MESSAGE_BYTES {
                // Drop the tail without buffering it, so the next read resyncs
                // on the following line instead of mid-message.
                self.skip_to_newline()?;
                return Err(TransportError::MessageTooLarge {
                    max: MAX_MESSAGE_BYTES,
                });
            }
            let line = String::from_utf8(buffer).map_err(|_| TransportError::NotUtf8)?;
            if !line.trim().is_empty() {
                return Ok(Some(line));
            }
        }
    }

    fn skip_to_newline(&mut self) -> io::Result<()> {
        loop {
            let (found, used) = {
                let available = self.reader.fill_buf()?;
                if available.is_empty() {
                    return Ok(());
                }
                match available.iter().position(|byte| *byte == b'\n') {
                    Some(index) => (true, index + 1),
                    None => (false, available.len()),
                }
            };
            self.reader.consume(used);
            if found {
                return Ok(());
            }
        }
    }
}

/// Request id of a message that could not be decoded far enough to correlate.
fn uncorrelated() -> RequestId {
    RequestId::from_uuid(Uuid::nil())
}

/// Best-effort correlation for a rejected line: the envelope id if it parsed.
fn recover_request_id(line: &str) -> RequestId {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|raw| {
            raw.get("request_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|id| id.parse().ok())
        })
        .unwrap_or_else(uncorrelated)
}

fn failure(request_id: RequestId, error: &TransportError) -> ServerEvent {
    ServerEvent::Failed {
        request_id,
        code: error.code().to_owned(),
        message: error.to_string(),
    }
}

#[derive(Debug)]
pub enum TransportError {
    Io(io::Error),
    Protocol(ProtocolError),
    Secret(SecretError),
    MessageTooLarge { max: usize },
    NotUtf8,
}

impl TransportError {
    /// Stable machine-readable code carried on a `failed` event.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::Protocol(error) => match error {
                ProtocolError::UnsupportedVersion { .. } => "unsupported_version",
                ProtocolError::UnknownVariant { .. } => "unknown_variant",
                ProtocolError::Malformed(_) => "malformed",
                ProtocolError::InvalidIdempotencyKey => "invalid_idempotency_key",
                ProtocolError::IdempotencyConflict { .. } => "idempotency_conflict",
                ProtocolError::SequenceGap { .. } => "sequence_gap",
                ProtocolError::SequenceOverflow => "sequence_overflow",
            },
            Self::Secret(error) => error.code(),
            Self::MessageTooLarge { .. } => "message_too_large",
            Self::NotUtf8 => "not_utf8",
        }
    }
}

impl From<SecretError> for TransportError {
    fn from(value: SecretError) -> Self {
        Self::Secret(value)
    }
}

impl From<io::Error> for TransportError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<ProtocolError> for TransportError {
    fn from(value: ProtocolError) -> Self {
        Self::Protocol(value)
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "transport io: {error}"),
            Self::Protocol(error) => error.fmt(formatter),
            Self::Secret(error) => error.fmt(formatter),
            Self::MessageTooLarge { max } => {
                write!(formatter, "message exceeds the {max}-byte line limit")
            }
            Self::NotUtf8 => formatter.write_str("message is not valid UTF-8"),
        }
    }
}

impl std::error::Error for TransportError {}
