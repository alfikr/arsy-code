//! The reference thin client: an editor's half of an ACP session.
//!
//! # What "thin" means here, precisely
//!
//! This crate depends on `serde_json` and nothing else — not on `arsy-code`,
//! not on `arsy-kernel`. It cannot compile a prompt, evaluate a policy, run a
//! tool, or reach a model, because it has no code that could. Everything it
//! knows about a session it learned from a message on a pipe.
//!
//! That is the point of building it: an integration that *could* reach into
//! the runtime eventually does, and then the editor and the terminal disagree
//! about what a session is. A client that cannot even link against the runtime
//! is one that stays honest by construction.
//!
//! # What it does
//!
//! ```text
//! initialize            what the agent supports
//! session/new           a session id
//! session/prompt        one turn, streamed back as session/update
//! session/cancel        stop what has not started
//! ```
//!
//! Rendering is the whole of its intelligence: turn a notification into a line
//! of text, and a response into a status. Anything it does not recognize is
//! shown as what it is rather than dropped, because an editor that silently
//! ignores an update is worse than one that prints a shrug.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// What the agent said while answering, and what it finally answered.
#[derive(Debug, Eq, PartialEq)]
pub struct Answer {
    /// Rendered `session/update` notifications, in arrival order.
    pub updates: Vec<String>,
    /// The response's `result`, or the error it carried instead.
    pub outcome: Result<Value, Value>,
}

impl Answer {
    /// Everything the agent streamed, joined — what an editor shows in the
    /// conversation pane.
    pub fn text(&self) -> String {
        self.updates.concat()
    }
}

/// One connection to an agent speaking ACP over a pair of pipes.
pub struct Client<R, W> {
    reader: R,
    writer: W,
    id: u64,
}

impl<R: BufRead, W: Write> Client<R, W> {
    pub const fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            id: 0,
        }
    }

    /// Send one request and read until its response, rendering whatever the
    /// agent streamed on the way.
    ///
    /// Correlation is by id rather than by order: a notification and a
    /// response are the same kind of line on the same pipe, and an editor that
    /// assumed the next line was its answer would show a chunk of streamed
    /// text as a result.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Answer, String> {
        self.id += 1;
        let id = self.id;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.writer, "{request}").map_err(|error| error.to_string())?;
        self.writer.flush().map_err(|error| error.to_string())?;

        let mut updates = Vec::new();
        loop {
            let mut line = String::new();
            if self
                .reader
                .read_line(&mut line)
                .map_err(|error| error.to_string())?
                == 0
            {
                return Err(format!("the agent closed the connection during {method}"));
            }
            if line.trim().is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(line.trim())
                .map_err(|error| format!("the agent sent something that is not JSON: {error}"))?;
            if message.get("id") == Some(&json!(id)) {
                let outcome = match message.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                return Ok(Answer { updates, outcome });
            }
            if let Some(rendered) = render(&message) {
                updates.push(rendered);
            }
        }
    }
}

/// One notification as a line an editor can show, or `None` when it carries
/// nothing a person would read.
pub fn render(message: &Value) -> Option<String> {
    if message.get("method")?.as_str()? != "session/update" {
        return None;
    }
    let update = message.get("params")?.get("update")?;
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("agent_message_chunk") => update
            .get("content")?
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned),
        // An update this client does not render is still shown, named, rather
        // than dropped where nobody can tell it arrived.
        Some(kind) => Some(format!("[{kind}]")),
        None => Some("[update]".to_owned()),
    }
}

/// The prompt shape ACP expects: content blocks, of which this client sends
/// one.
pub fn prompt(session: &str, text: &str) -> Value {
    json!({
        "sessionId": session,
        "prompt": [{"type": "text", "text": text}],
    })
}

/// The session id out of a `session/new` result.
pub fn session_id(result: &Value) -> Option<&str> {
    result.get("sessionId").and_then(Value::as_str)
}

/// A JSON-RPC error as one line, keeping the agent's own diagnostic code when
/// it sent one: the editor and the terminal should name a failure the same way.
pub fn explain(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("the agent refused the request");
    match error.get("data").and_then(|data| data.get("arsyCode")) {
        Some(code) => format!("{}: {message}", code.as_str().unwrap_or_default()),
        None => message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An agent that answers from a script, so the client is tested against
    /// the message shapes rather than against a process.
    fn client(script: &str) -> Client<&[u8], Vec<u8>> {
        Client::new(script.as_bytes(), Vec::new())
    }

    #[test]
    fn streamed_chunks_arrive_before_the_response_and_are_not_mistaken_for_it() {
        let mut client = client(concat!(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"the answer "}}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"is 42."}}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}"#,
            "\n",
        ));

        let answer = client.call("session/prompt", prompt("s", "what?")).unwrap();

        assert_eq!(answer.text(), "the answer is 42.[tool_call]");
        assert_eq!(answer.outcome.unwrap()["stopReason"], "end_turn");
    }

    #[test]
    fn an_error_keeps_the_code_the_terminal_would_have_printed() {
        let mut client = client(concat!(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"no credential","data":{"arsyCode":"ARSY-PRV-1000"}}}"#,
            "\n",
        ));

        let answer = client.call("session/prompt", prompt("s", "go")).unwrap();

        let error = answer.outcome.unwrap_err();
        assert_eq!(explain(&error), "ARSY-PRV-1000: no credential");
    }

    #[test]
    fn a_connection_that_ends_mid_turn_is_reported_rather_than_hung_on() {
        let mut client = client("");
        let error = client.call("initialize", json!({})).unwrap_err();
        assert!(error.contains("closed the connection"), "{error}");
    }

    #[test]
    fn every_request_carries_its_own_id_so_answers_can_be_matched() {
        let mut client = Client::new(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"abc"}}"#,
                "\n",
            )
            .as_bytes(),
            Vec::new(),
        );

        client.call("initialize", json!({})).unwrap();
        let opened = client.call("session/new", json!({})).unwrap();

        assert_eq!(session_id(&opened.outcome.unwrap()), Some("abc"));
    }
}
