//! The HTTP call a provider adapter makes, over TLS.
//!
//! This is the only place in the kernel that reaches the network. Everything
//! specific to a provider — body shape, headers, status mapping, SSE decoding —
//! stays in the adapter, so this module has no idea which provider it is
//! talking to and needs no change when one is added.

use super::{
    wire::{WireRequest, WireResponse, WireTransport},
    ProviderError,
};
use std::{
    io::{BufRead, BufReader, Read},
    time::Duration,
};

pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Time allowed for the response head. The body is a stream that legitimately
/// stays open for minutes, so it is not bounded by a deadline.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);

/// Longest single line accepted from a response.
///
/// One server-sent event is one line, so a well-behaved provider stays far
/// below this. The bound exists because the endpoint is operator-configured
/// and may be anything: without it, a host that never sends a newline would
/// grow the buffer until the process dies.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

pub struct HttpTransport {
    agent: ureq::Agent,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new(DEFAULT_CONNECT_TIMEOUT, DEFAULT_RESPONSE_TIMEOUT)
    }
}

impl HttpTransport {
    pub fn new(connect: Duration, response: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            // A non-2xx response carries the provider's own error body, which
            // the adapter needs in order to normalize it. Letting ureq turn a
            // status into an error would throw that body away.
            .http_status_as_error(false)
            .timeout_connect(Some(connect))
            .timeout_recv_response(Some(response))
            .user_agent(concat!("arsy/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl WireTransport for HttpTransport {
    fn send(&self, request: WireRequest) -> Result<WireResponse, ProviderError> {
        require_transport_security(&request.url)?;
        let mut builder = self.agent.post(&request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let response = builder
            .send(request.body.as_bytes())
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        Ok(WireResponse {
            status,
            headers,
            lines: Box::new(Lines::new(response.into_body().into_reader())),
        })
    }
}

/// Refuse to send a credential in the clear.
///
/// A loopback host is exempt because that is how a local runtime — Ollama,
/// LM Studio, llama.cpp — is reached, and the traffic never leaves the machine.
fn require_transport_security(url: &str) -> Result<(), ProviderError> {
    if url.starts_with("https://") {
        return Ok(());
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| ProviderError::InvalidRequest(format!("unsupported URL scheme: {url}")))?;
    if is_loopback(host_of(rest)) {
        return Ok(());
    }
    Err(ProviderError::InvalidRequest(format!(
        "refusing to send a credential over plaintext http to a non-loopback host: {url}"
    )))
}

/// Authority section of a URL with its scheme already removed, minus userinfo
/// and port.
fn host_of(rest: &str) -> &str {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, it)| it);
    match host.strip_prefix('[') {
        // IPv6 literal: the port, if any, follows the closing bracket.
        Some(inner) => inner.split_once(']').map_or(inner, |(it, _)| it),
        None => host.split_once(':').map_or(host, |(it, _)| it),
    }
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Line reader with a bound, so an endpoint cannot force unbounded buffering.
///
/// `BufRead::lines` has no such bound, which is the only reason this exists.
struct Lines<R> {
    reader: BufReader<R>,
    buffer: Vec<u8>,
    done: bool,
}

impl<R: Read> Lines<R> {
    fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            buffer: Vec::new(),
            done: false,
        }
    }
}

impl<R: Read> Iterator for Lines<R> {
    type Item = Result<String, String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        self.buffer.clear();
        // `read_until` appends, so the cap is checked against what one call
        // produced rather than against a growing total.
        let read = match (&mut self.reader)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut self.buffer)
        {
            Ok(0) => {
                self.done = true;
                return None;
            }
            Ok(read) => read,
            Err(error) => {
                self.done = true;
                return Some(Err(error.to_string()));
            }
        };
        if read > MAX_LINE_BYTES {
            self.done = true;
            return Some(Err(format!(
                "response line exceeded {MAX_LINE_BYTES} bytes"
            )));
        }
        while self
            .buffer
            .last()
            .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
        {
            self.buffer.pop();
        }
        Some(
            String::from_utf8(std::mem::take(&mut self.buffer))
                .map_err(|error| format!("response line is not UTF-8: {error}")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_is_refused_except_on_loopback() {
        for allowed in [
            "https://api.anthropic.com/v1/messages",
            "http://localhost:11434/v1/chat/completions",
            "http://127.0.0.1:1234/v1/chat/completions",
            "http://[::1]:1234/v1/chat/completions",
            "http://user@127.0.0.1:1234/v1",
        ] {
            require_transport_security(allowed).unwrap_or_else(|error| {
                panic!("{allowed} should be allowed, got {error}");
            });
        }
        for refused in [
            "http://api.openai.com/v1",
            // Not loopback: an attacker-controlled name that merely starts
            // with one.
            "http://localhost.attacker.test/v1",
            "http://127.0.0.1.attacker.test/v1",
            "ftp://example.test/v1",
        ] {
            assert!(
                matches!(
                    require_transport_security(refused),
                    Err(ProviderError::InvalidRequest(_))
                ),
                "{refused} should be refused"
            );
        }
    }

    #[test]
    fn lines_split_on_newlines_and_refuse_an_unbounded_one() {
        let body = "event: a\r\ndata: {}\n\nlast";
        let lines: Vec<_> = Lines::new(body.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(lines, ["event: a", "data: {}", "", "last"]);

        let huge = vec![b'x'; MAX_LINE_BYTES + 1];
        let mut reader = Lines::new(huge.as_slice());
        assert!(reader.next().unwrap().is_err());
        assert!(reader.next().is_none(), "the reader stops after the bound");
    }
}
