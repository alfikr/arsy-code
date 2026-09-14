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
    process::{Command, Stdio},
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
        Self::scripted(script.into_iter().map(Some).collect())
    }

    /// A `None` entry accepts the request and never answers, which is what a
    /// client sees when the process holding the turn is killed.
    fn scripted(script: Vec<Option<String>>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, bodies) = mpsc::channel();
        thread::spawn(move || {
            let mut held = Vec::new();
            for body in script {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut stream = stream;
                let request = read_request(&mut stream);
                // A receiver that has gone away means the test finished early;
                // the response still goes out so the client is not left hanging.
                let _ = sender.send(request);
                let Some(body) = body else {
                    // Hold the connection open without answering, and keep
                    // accepting: the test kills the client, then connects
                    // again to resume.
                    held.push(stream);
                    continue;
                };
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
/// A `PreToolUse` guard on `fs.read` whose hook denies the call with `reason`.
///
/// The denial is written to a file in `directory` and the hook prints it, so
/// the command carries no quotes for a shell to disagree about.
fn denying_guard(directory: &Path, reason: &str) -> String {
    let answer = directory.join("deny.json");
    std::fs::write(
        &answer,
        format!(r#"{{"decision": "deny", "reason": "{reason}"}}"#),
    )
    .unwrap();
    // Printing the file rather than the JSON keeps every quote out of the
    // command line. `cmd /C` and `sh -c` disagree about quoting in ways no
    // single string satisfies: sh strips the double quotes the JSON needs, and
    // cmd strips them too once they reach it through Rust's own argument
    // quoting.
    let command = if cfg!(windows) {
        format!("type {}", answer.display())
    } else {
        format!("cat {}", answer.display())
    };
    serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "fs.read",
                "hooks": [{"type": "command", "command": command}]
            }]
        }
    })
    .to_string()
}

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
    arsy_with_home(workspace, home, home, args)
}

/// The same run with the operator's home under the test's control, so the
/// hooks the engine finds are the ones the test wrote.
fn arsy_with_home(
    workspace: &Path,
    config_home: &Path,
    home: &Path,
    args: &[&str],
) -> (i32, Vec<Value>) {
    let output = Command::new(env!("CARGO_BIN_EXE_arsy"))
        .args(["--workspace", workspace.to_str().unwrap()])
        .args(args)
        .args(["--output", "json"])
        .env("ARSY_CONFIG_HOME", config_home)
        .env("HOME", home)
        .env("USERPROFILE", home)
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

#[test]
fn a_task_its_process_never_finished_is_continued_by_resume() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "the answer is 42\n").unwrap();

    // The first request is accepted and never answered, so the run is still
    // holding its task when the test kills it -- a process that died mid-turn.
    let provider = FakeProvider::scripted(vec![
        None,
        Some(asks_to_read("notes.txt")),
        Some(answers("42.")),
    ]);
    configure(home.path(), provider.port);

    let mut child = Command::new(env!("CARGO_BIN_EXE_arsy"))
        .args(["--workspace", workspace.path().to_str().unwrap()])
        .args(["run", "what does notes.txt say?"])
        .args(["--output", "json"])
        .env("ARSY_CONFIG_HOME", home.path())
        .env("ARSY_TEST_KEY", "test-key-0123456789abcdef")
        .stdout(Stdio::null())
        .spawn()
        .expect("the binary runs");
    // The provider has the request, so the turn and its task are on the record.
    drop(provider.request());
    child.kill().expect("the run is killed mid-turn");
    child.wait().expect("the killed run is reaped");

    let (code, listed) = arsy(workspace.path(), home.path(), &["session", "list"]);
    assert_eq!(code, 0);
    let listed = result(&listed);
    let session = listed["sessions"][0]["session"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(listed["sessions"][0]["status"], "running");

    // Resume closes the open turn, takes the task back, and finishes it with
    // the goal it was created with -- which nothing but the store still knows.
    let (code, records) = arsy(workspace.path(), home.path(), &["resume", &session]);
    let resumed = result(&records);
    assert_eq!(code, 0, "{resumed:#?}");
    assert_eq!(resumed["closed_turns"], 1);
    assert_eq!(resumed["recovered_tasks"], 1);
    assert_eq!(resumed["continuing"], resumed["task"]);
    assert_eq!(resumed["status"], "completed");
    assert_eq!(resumed["telemetry"]["tool_calls"], 1);

    // The resumed turn asked the same question the killed one did.
    let continued = provider.request().to_string();
    assert!(
        continued.contains("what does notes.txt say?"),
        "{continued}"
    );

    // Nothing is left waiting once it completed.
    let (code, records) = arsy(workspace.path(), home.path(), &["resume", &session]);
    assert_eq!(code, 0);
    let quiet = result(&records);
    assert_eq!(quiet["continuing"], Value::Null);
    assert_eq!(quiet["recovered_tasks"], 0);
}

#[test]
fn what_the_workspace_remembers_reaches_the_model_and_can_be_withdrawn() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let provider = FakeProvider::serving(vec![answers("noted."), answers("noted again.")]);
    configure(home.path(), provider.port);

    let (code, records) = arsy(
        workspace.path(),
        home.path(),
        &[
            "memory",
            "remember",
            "the test command is `cargo nextest run`",
        ],
    );
    assert_eq!(code, 0, "{records:#?}");
    let memory = result(&records)["memory"].as_str().unwrap().to_owned();

    let (code, records) = arsy(workspace.path(), home.path(), &["memory", "list"]);
    assert_eq!(code, 0);
    let listed = result(&records);
    assert_eq!(listed["memories"].as_array().unwrap().len(), 1);
    assert_eq!(listed["memories"][0]["scope"], "repository");
    assert_eq!(
        listed["memories"][0]["claim"],
        "the test command is `cargo nextest run`"
    );
    // Typed at a prompt with nothing checking it: reported, whatever authority
    // the operator has.
    assert_eq!(listed["memories"][0]["confidence"], "reported");

    let (code, _) = arsy(workspace.path(), home.path(), &["run", "how do I test?"]);
    assert_eq!(code, 0);
    let asked = provider.request().to_string();
    assert!(asked.contains("cargo nextest run"), "{asked}");

    // Withdrawn, and the next turn is not told it.
    let (code, _) = arsy(
        workspace.path(),
        home.path(),
        &[
            "memory",
            "forget",
            &memory,
            "--to",
            "we moved back to cargo test",
        ],
    );
    assert_eq!(code, 0);
    let (code, _) = arsy(workspace.path(), home.path(), &["run", "how do I test?"]);
    assert_eq!(code, 0);
    let asked = provider.request().to_string();
    assert!(!asked.contains("cargo nextest run"), "{asked}");

    // The tombstone stays, with the reason.
    let (_, records) = arsy(workspace.path(), home.path(), &["memory", "list", "--all"]);
    let all = result(&records);
    assert_eq!(all["memories"][0]["status"], "revoked");
    assert_eq!(
        all["memories"][0]["revocation"],
        "we moved back to cargo test"
    );
}

/// A reply that asks for one subagent and stops.
fn spawns(goal: &str, capabilities: &str) -> String {
    sse(&[
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call-spawn",
            "type": "function",
            "function": {
                "name": "task.spawn",
                "arguments": format!("{{\"goal\": \"{goal}\", \"capabilities\": [{capabilities}]}}")
            }
        }]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

/// A child reply that asks to write, which its own grants must refuse.
fn tries_to_write() -> String {
    sse(&[
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call-write",
            "type": "function",
            "function": {"name": "fs.write", "arguments": "{\"path\": \"escaped.txt\", \"content\": \"x\"}"}
        }]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

#[test]
fn a_subagent_holds_less_authority_than_the_parent_that_spawned_it() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "the answer is 42\n").unwrap();

    // Parent asks for a subagent; the child reads, then tries to write, then
    // answers. The parent reports what the child said.
    let provider = FakeProvider::serving(vec![
        spawns("what does notes.txt say", "\"fs.read\""),
        asks_to_read("notes.txt"),
        tries_to_write(),
        answers("notes.txt says 42."),
        answers("the subagent found 42."),
    ]);
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "schema_version = 1\n\
             [provider.endpoint.local]\n\
             kind = \"openai\"\n\
             base_url = \"http://127.0.0.1:{}\"\n\
             model = \"test-model\"\n\
             api_key_env = \"ARSY_TEST_KEY\"\n\
             # Delegation is off unless a rule says otherwise, so the depth is\n\
             # what makes a subagent possible at all.\n\
             [[policy.rules]]\n\
             id = \"delegate-reads\"\n\
             effect = \"allow\"\n\
             action = \"fs.read\"\n\
             resource = \"file:**\"\n\
             delegation_depth = 2\n\
             [[policy.rules]]\n\
             id = \"parent-writes\"\n\
             effect = \"allow\"\n\
             action = \"fs.write\"\n\
             resource = \"file:**\"\n",
            provider.port
        ),
    )
    .unwrap();

    let (code, records) = arsy(
        workspace.path(),
        home.path(),
        &["run", "find out what notes.txt says"],
    );
    let result = result(&records);
    assert_eq!(code, 0, "{result:#?}");
    assert_eq!(result["status"], "completed");

    // The parent was offered the spawn tool, because this policy delegates.
    let first = provider.request();
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert!(tools.contains(&"task.spawn"), "{tools:?}");

    // What the spawn call actually returned, so a failure here says why.
    let second = provider.request();
    let spawn_result = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .unwrap_or_default();
    assert!(
        !spawn_result.starts_with("error:"),
        "the spawn failed: {spawn_result}"
    );

    // The child was offered the workspace tools but holds only what was
    // delegated: its write was refused by its own runtime, not by the parent's.
    drop(provider.request()); // the child has the file and asks to write
    let refused = provider.request();
    let messages = refused["messages"].as_array().unwrap();
    let tool_result = messages
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .expect("the child was told what happened");
    let text = tool_result["content"].as_str().unwrap();
    assert!(text.starts_with("error:"), "{text}");
    assert!(
        !workspace.path().join("escaped.txt").exists(),
        "a subagent must not be able to write"
    );

    // The parent's last request carries the child's answer as a tool result.
    let parent = provider.request().to_string();
    assert!(parent.contains("notes.txt says 42."), "{parent}");
}

#[test]
fn a_claim_that_looks_like_a_credential_is_never_written_to_the_artifact_store() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let provider = FakeProvider::serving(vec![answers("noted.")]);
    configure(home.path(), provider.port);

    let artifacts = workspace.path().join(".arsy/artifacts");
    let count = || -> usize { walk(&artifacts).len() };

    // A claim that is fine is stored, so the comparison below is against a
    // store that is working rather than one that never writes.
    let (code, _) = arsy(
        workspace.path(),
        home.path(),
        &["memory", "remember", "the build uses cargo"],
    );
    assert_eq!(code, 0);
    let after_good = count();
    assert!(after_good > 0, "an accepted claim is stored");

    // A claim carrying something shaped like a key is refused -- and nothing
    // is left behind, because a memory claim is kept alive by retention rather
    // than by reachability, so an orphan here is uncollectable forever.
    let (code, refused) = arsy(
        workspace.path(),
        home.path(),
        &[
            "memory",
            "remember",
            "the deploy token is ghp_0123456789abcdefghijklmnopqrstuvwxyz",
        ],
    );
    assert_eq!(code, 3, "{refused:#?}");
    assert_eq!(
        count(),
        after_good,
        "a refused claim wrote a blob that nothing can collect"
    );
}

/// Every file under a directory, if it exists.
fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            found.extend(walk(&entry.path()));
        } else {
            found.push(entry.path());
        }
    }
    found
}

/// A hook that denies a tool call, on a real turn, from the operator's own
/// file — the whole path from a declaration on disk to a call that does not
/// happen.
#[test]
fn a_hook_denies_a_tool_call_and_the_model_is_told_why() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "the answer is 42\n").unwrap();
    std::fs::create_dir_all(home.path().join(".arsy")).unwrap();
    std::fs::write(
        home.path().join(".arsy/guard.json"),
        denying_guard(&home.path().join(".arsy"), "notes are off limits"),
    )
    .unwrap();

    let provider = FakeProvider::serving(vec![asks_to_read("notes.txt"), answers("I could not.")]);
    configure(home.path(), provider.port);

    let (code, records) = arsy_with_home(
        workspace.path(),
        home.path(),
        home.path(),
        &["run", "what does notes.txt say?"],
    );

    let result = result(&records);
    assert_eq!(code, 0, "{result:#?}");

    let _first = provider.request();
    // The second request carries what the tool call produced. The hook denied
    // it, so it carries the refusal and its reason — and not the file.
    let second = provider.request();
    let transcript = second.to_string();
    assert!(
        transcript.contains("notes are off limits"),
        "the model was told why: {transcript}"
    );
    assert!(
        !transcript.contains("the answer is 42"),
        "the file was never read: {transcript}"
    );
}

/// The same file in the repository rather than the operator's home does
/// nothing until the operator vouches for that directory.
#[test]
fn a_repositorys_own_hook_does_not_run_until_it_is_vouched_for() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "the answer is 42\n").unwrap();
    std::fs::create_dir_all(workspace.path().join(".arsy")).unwrap();
    std::fs::write(
        workspace.path().join(".arsy/guard.json"),
        denying_guard(&workspace.path().join(".arsy"), "the repo said no"),
    )
    .unwrap();

    let provider = FakeProvider::serving(vec![asks_to_read("notes.txt"), answers("42.")]);
    configure(home.path(), provider.port);

    let (code, records) = arsy_with_home(
        workspace.path(),
        home.path(),
        home.path(),
        &["run", "what does notes.txt say?"],
    );
    assert_eq!(code, 0, "{:#?}", result(&records));

    let _first = provider.request();
    let unvouched = provider.request().to_string();
    assert!(
        !unvouched.contains("the repo said no"),
        "an unvouched repository's hook did not run: {unvouched}"
    );
    assert!(
        unvouched.contains("the answer is 42"),
        "so the read happened: {unvouched}"
    );

    // Vouched for, the same file denies the same call.
    let provider = FakeProvider::serving(vec![asks_to_read("notes.txt"), answers("I could not.")]);
    configure_trusting(home.path(), provider.port, workspace.path());

    let (code, records) = arsy_with_home(
        workspace.path(),
        home.path(),
        home.path(),
        &["run", "what does notes.txt say?"],
    );
    assert_eq!(code, 0, "{:#?}", result(&records));
    let _first = provider.request();
    let vouched = provider.request().to_string();
    assert!(
        vouched.contains("the repo said no"),
        "vouched for, it runs: {vouched}"
    );
}

/// The same configuration, plus the operator vouching for one directory.
fn configure_trusting(home: &Path, port: u16, workspace: &Path) {
    configure(home, port);
    let mut config = std::fs::read_to_string(home.join("config.toml")).unwrap();
    config.push_str(&format!(
        // A literal key: a Windows path's backslashes are escapes in a TOML
        // basic string.
        "[project.'{}']\ntrust_level = \"trusted\"\n",
        workspace.display()
    ));
    std::fs::write(home.join("config.toml"), config).unwrap();
}
