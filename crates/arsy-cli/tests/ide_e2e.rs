//! The reference thin client against the real agent, over a real pipe.
//!
//! `arsy-ide` links against nothing but `serde_json`, so the only way to know
//! it and the agent agree is to run them together. This is that test: the
//! client's own `Client`, the binary's ACP loop, and a scripted provider on
//! loopback.

use arsy_ide::{explain, prompt, session_id, Client};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener},
    process::{Command, Stdio},
    thread,
};

fn provider(reply: String) -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
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
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .as_bytes(),
        );
    });
    port
}

fn sse(chunks: &[Value]) -> String {
    chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n"
}

#[test]
fn the_thin_client_drives_a_turn_through_the_protocol_and_nothing_else() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let port = provider(sse(&[
        json!({"choices": [{"delta": {"content": "hello from "}}]}),
        json!({"choices": [{"delta": {"content": "the agent."}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ]));
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "schema_version = 1\n\
             [provider.endpoint.local]\n\
             kind = \"openai\"\n\
             base_url = \"http://127.0.0.1:{port}\"\n\
             model = \"test-model\"\n\
             api_key_env = \"ARSY_TEST_KEY\"\n"
        ),
    )
    .unwrap();

    let mut agent = Command::new(env!("CARGO_BIN_EXE_arsy"))
        .args(["--workspace", workspace.path().to_str().unwrap()])
        .args(["serve", "--protocol", "acp"])
        .env("ARSY_CONFIG_HOME", home.path())
        .env("ARSY_TEST_KEY", "test-key-0123456789abcdef")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the agent runs");
    let mut client = Client::new(
        BufReader::new(agent.stdout.take().unwrap()),
        agent.stdin.take().unwrap(),
    );

    let capabilities = client
        .call("initialize", json!({"protocolVersion": 1}))
        .unwrap()
        .outcome
        .expect("the agent initializes");
    assert_eq!(capabilities["protocolVersion"], 1);

    let opened = client
        .call("session/new", json!({"cwd": workspace.path()}))
        .unwrap()
        .outcome
        .expect("a session opens");
    let session = session_id(&opened)
        .expect("the session is named")
        .to_owned();

    let answered = client
        .call("session/prompt", prompt(&session, "say hello"))
        .unwrap();
    assert_eq!(answered.text(), "hello from the agent.");
    assert_eq!(
        answered.outcome.expect("the turn ends")["stopReason"],
        "end_turn"
    );

    // A refusal reaches the client as the same diagnostic code the terminal
    // would have printed.
    let unknown = client
        .call(
            "session/load",
            json!({"sessionId": "00000000-0000-4000-8000-000000000000"}),
        )
        .unwrap();
    assert_eq!(
        explain(&unknown.outcome.expect_err("an unknown session is refused")),
        "ARSY-SCH-1004: session 00000000-0000-4000-8000-000000000000 has no recorded events here"
    );

    drop(client);
    let _ = agent.wait();
}
