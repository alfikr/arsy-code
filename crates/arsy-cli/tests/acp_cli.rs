//! `arsy serve --protocol acp` as an editor drives it: initialize, open a
//! session, prompt, watch the answer stream in, and cancel.
//!
//! The provider is the same loopback fake the run tests use, so this exercises
//! the whole path an IDE takes -- protocol, session, task graph, tool runtime,
//! policy -- rather than the adapter's translation in isolation.

use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
};

fn sse(chunks: &[Value]) -> String {
    chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n"
}

/// One provider reply that streams two text chunks and stops.
fn answers(first: &str, second: &str) -> String {
    sse(&[
        json!({"choices": [{"delta": {"content": first}}]}),
        json!({"choices": [{"delta": {"content": second}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        json!({"usage": {"prompt_tokens": 11, "completion_tokens": 3}}),
    ])
}

fn provider(script: Vec<String>) -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for body in script {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(&mut stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut discard = vec![0; length];
            let _ = reader.read_exact(&mut discard);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    port
}

/// The editor's end of the pipe.
struct Editor {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    id: u64,
}

impl Editor {
    fn open(workspace: &Path, home: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_arsy"))
            .args(["--workspace", workspace.to_str().unwrap()])
            .args(["serve", "--protocol", "acp"])
            .env("ARSY_CONFIG_HOME", home)
            .env("ARSY_TEST_KEY", "test-key-0123456789abcdef")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the binary runs");
        let reader = BufReader::new(child.stdout.take().expect("stdout is piped"));
        Self {
            child,
            reader,
            id: 0,
        }
    }

    /// Send one request and read messages until its response arrives, handing
    /// back the notifications that came first.
    fn call(&mut self, method: &str, params: Value) -> (Vec<Value>, Value) {
        self.id += 1;
        let id = self.id;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.child.stdin.as_mut().expect("stdin is piped");
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();

        let mut notifications = Vec::new();
        loop {
            let mut line = String::new();
            assert!(
                self.reader.read_line(&mut line).unwrap() > 0,
                "the server closed the pipe before answering {method}"
            );
            let message: Value = serde_json::from_str(line.trim()).expect("a JSON-RPC message");
            if message["id"] == json!(id) {
                return (notifications, message);
            }
            notifications.push(message);
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        // Closing the pipe is what stops the loop; killing is the fallback for
        // a test that failed mid-conversation.
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

#[test]
fn an_editor_initializes_opens_a_session_prompts_and_sees_the_answer_stream() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let port = provider(vec![answers("the answer ", "is 42.")]);
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "schema_version = 1\n\
             [provider.endpoint.local]\n\
             kind = \"openai\"\n\
             base_url = \"http://127.0.0.1:{port}\"\n\
             model = \"test-model\"\n\
             api_key_env = \"ARSY_TEST_KEY\"\n\
             [policy]\n\
             default_effect = \"allow\"\n"
        ),
    )
    .unwrap();
    let mut editor = Editor::open(workspace.path(), home.path());

    let (_, initialized) = editor.call("initialize", json!({"protocolVersion": 1}));
    assert_eq!(initialized["result"]["protocolVersion"], 1);
    assert_eq!(
        initialized["result"]["agentCapabilities"]["loadSession"],
        true
    );

    let (_, opened) = editor.call("session/new", json!({"cwd": workspace.path()}));
    let session = opened["result"]["sessionId"].as_str().unwrap().to_owned();

    let (updates, answered) = editor.call(
        "session/prompt",
        json!({
            "sessionId": session,
            "prompt": [{"type": "text", "text": "what is the answer?"}],
        }),
    );
    assert_eq!(answered["result"]["stopReason"], "end_turn");
    // The reply arrived as it was produced, not in one lump at the end.
    let streamed: String = updates
        .iter()
        .filter(|update| update["method"] == "session/update")
        .filter_map(|update| update["params"]["update"]["content"]["text"].as_str())
        .collect();
    assert_eq!(streamed, "the answer is 42.");
    assert!(updates
        .iter()
        .all(|update| update["params"]["sessionId"] == json!(session.clone())));

    // The session an editor opened is a session every other surface knows.
    let (_, loaded) = editor.call("session/load", json!({"sessionId": session}));
    assert!(loaded["result"]["events"].as_u64().unwrap() > 0);

    // Nothing is left running, so there is nothing to cancel -- and saying
    // zero is the truthful answer rather than an error.
    let (_, cancelled) = editor.call("session/cancel", json!({"sessionId": session}));
    assert_eq!(cancelled["result"]["cancelled"], 0);

    // A method this build does not implement is refused with the protocol's
    // own code, not a crash.
    let (_, refused) = editor.call("session/dance", json!({}));
    assert_eq!(refused["error"]["code"], -32601);

    // A session this workspace never recorded is refused rather than invented.
    let (_, unknown) = editor.call(
        "session/load",
        json!({"sessionId": "00000000-0000-4000-8000-000000000000"}),
    );
    assert_eq!(unknown["error"]["data"]["arsyCode"], "ARSY-SCH-1004");
}
