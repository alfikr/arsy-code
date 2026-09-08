//! One scripted turn, end to end, against a provider on loopback.
//!
//! This is the only test that exercises the whole spine at once —
//! configuration, credential, routing, provider dialect, the tool runtime,
//! policy, the event store, and telemetry — so a change that breaks the seam
//! between two of them fails here rather than in production. Everything is
//! local: the "provider" is a socket this test owns, and the model's turn is a
//! script, so there is nothing to be flaky about.

use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener},
    path::Path,
    process::Command,
    sync::mpsc,
    thread,
};

/// A fake OpenAI-dialect endpoint that replays one scripted response per
/// request and hands back the request bodies it was sent.
struct FakeProvider {
    port: u16,
    bodies: mpsc::Receiver<String>,
}

impl FakeProvider {
    /// `script` holds one SSE body per expected request, in order.
    fn serving(script: Vec<String>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, bodies) = mpsc::channel();
        thread::spawn(move || {
            for body in script {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut stream = stream;
                let request = read_request(&mut stream);
                // A receiver that has gone away means the test finished early;
                // the response still goes out so the client is not left hanging.
                let _ = sender.send(request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self { port, bodies }
    }

    fn request(&self) -> Value {
        serde_json::from_str(&self.bodies.recv().expect("the provider was called")).unwrap()
    }
}

/// Read one HTTP request and return its body.
fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut reader = BufReader::new(stream);
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
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    String::from_utf8(body).unwrap()
}

/// One SSE body from its `data:` payloads.
fn sse(chunks: &[Value]) -> String {
    chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n"
}

fn asks_to_read(path: &str) -> String {
    sse(&[
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call-1",
            "type": "function",
            "function": {"name": "fs.read", "arguments": format!("{{\"path\": \"{path}\"}}")}
        }]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        serde_json::json!({"usage": {"prompt_tokens": 40, "completion_tokens": 8}}),
    ])
}

fn answers(text: &str) -> String {
    sse(&[
        serde_json::json!({"choices": [{"delta": {"content": text}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        serde_json::json!({"usage": {"prompt_tokens": 60, "completion_tokens": 12}}),
    ])
}

fn configure(home: &Path, port: u16) {
    std::fs::write(
        home.join("config.toml"),
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
}

fn arsy(workspace: &Path, home: &Path, args: &[&str]) -> (i32, Vec<Value>) {
    let output = Command::new(env!("CARGO_BIN_EXE_arsy"))
        .args(["--workspace", workspace.to_str().unwrap()])
        .args(args)
        .args(["--output", "json"])
        .env("ARSY_CONFIG_HOME", home)
        // Long enough that the redactor accepts it: a value short enough to
        // appear in ordinary text cannot be redacted safely and is refused.
        .env("ARSY_TEST_KEY", "test-key-0123456789abcdef")
        .output()
        .expect("the binary runs");
    let stdout = String::from_utf8(output.stdout).expect("machine output is UTF-8");
    let records = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    (output.status.code().unwrap_or(-1), records)
}

fn result(records: &[Value]) -> &Value {
    records
        .iter()
        .find(|record| record["type"] == "result")
        .map(|record| &record["payload"])
        .unwrap_or_else(|| panic!("no result record in {records:#?}"))
}

#[test]
fn a_scripted_turn_reads_a_file_answers_and_reports_what_it_spent() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "the answer is 42\n").unwrap();

    let provider = FakeProvider::serving(vec![asks_to_read("notes.txt"), answers("42.")]);
    configure(home.path(), provider.port);

    let (code, records) = arsy(
        workspace.path(),
        home.path(),
        &["run", "what does notes.txt say?"],
    );

    let result = result(&records);
    assert_eq!(code, 0, "{result:#?}");
    assert_eq!(result["status"], "completed");
    assert_eq!(result["provider"], "local");

    // The first request carried the task and the tool schemas; the second
    // carried the file the tool actually read, which is the whole point of the
    // loop — the model's next request sees the effect of its last call.
    let first = provider.request();
    assert_eq!(first["model"], "test-model");
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert!(tools.contains(&"fs.read"), "{tools:?}");
    let second = provider.request().to_string();
    assert!(second.contains("the answer is 42"), "{second}");

    let telemetry = &result["telemetry"];
    assert_eq!(telemetry["stop"], "answered");
    assert_eq!(telemetry["model_calls"], 2);
    assert_eq!(telemetry["tool_calls"], 1);
    assert_eq!(telemetry["tool_failures"], 0);
    assert_eq!(telemetry["input_tokens"], 100);
    assert_eq!(telemetry["output_tokens"], 20);
    assert_eq!(telemetry["retries"], 0);
    assert_eq!(telemetry["dropped"], 0);
    // Nothing was configured to export to, so nothing left the machine.
    assert_eq!(telemetry["exported"], 0);

    // What the turn spent is in the store, so `arsy session show` answers
    // "what did that run cost" after the process has exited.
    let session = result["session"].as_str().unwrap();
    let (code, shown) = arsy(
        workspace.path(),
        home.path(),
        &["session", "show", session, "--turns"],
    );
    assert_eq!(code, 0);
    let shown = self::result(&shown);
    assert_eq!(shown["turns"], 1);
    assert_eq!(shown["turn_detail"][0]["status"], "completed");
    assert_eq!(shown["usage"]["input_tokens"], 100);
    assert_eq!(shown["usage"]["output_tokens"], 20);
}

#[test]
fn a_refused_tool_reaches_the_model_as_a_failed_result_and_is_counted() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    // The file does not exist, so the call fails inside the runtime rather
    // than at the policy gate: the turn must continue and the model must be
    // told, because a provider that sent a call and never sees its result
    // rejects the next request.
    let provider = FakeProvider::serving(vec![asks_to_read("missing.txt"), answers("no file.")]);
    configure(home.path(), provider.port);

    let (code, records) = arsy(workspace.path(), home.path(), &["run", "read missing.txt"]);

    let result = result(&records);
    assert_eq!(code, 0, "{result:#?}");
    assert_eq!(result["telemetry"]["tool_calls"], 1);
    assert_eq!(result["telemetry"]["tool_failures"], 1);

    drop(provider.request());
    let second = provider.request();
    let messages = second["messages"].as_array().unwrap();
    let tool_result = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("the failed call was reported back");
    assert!(
        tool_result["content"]
            .as_str()
            .unwrap()
            .starts_with("error:"),
        "{tool_result}"
    );
}
