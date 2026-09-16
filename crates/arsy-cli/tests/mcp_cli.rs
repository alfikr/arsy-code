//! `arsy mcp` end to end: define a connection, list it, probe a real stdio
//! server over a real pipe, then disable and remove it.
//!
//! The server is a small script rather than a mock, so the negotiation, the
//! newline framing, and the subprocess lifecycle are all genuinely exercised.

use serde_json::Value;
use std::{path::Path, process::Command};

/// The fixture server is `arsy serve` itself.
///
/// Using the build's own MCP server rather than a scripted one keeps the test
/// free of an interpreter — the suite runs on Windows too — and makes this a
/// round trip: the client under test negotiates with the server under test.
fn server_command() -> String {
    env!("CARGO_BIN_EXE_arsy").to_owned()
}

fn arsy(workspace: &Path, args: &[&str]) -> (i32, Value) {
    // Never the Claude Code setup of the machine running the test.
    arsy_with_claude(workspace, &workspace.join("no-claude-home"), args)
}

fn arsy_with_claude(workspace: &Path, claude_home: &Path, args: &[&str]) -> (i32, Value) {
    // Global flags go first: everything after a bare `--` belongs to the
    // connection's own command line, so appending them would hand ARSY's flags
    // to the server instead.
    let output = Command::new(env!("CARGO_BIN_EXE_arsy"))
        .args(["--workspace", workspace.to_str().unwrap()])
        .env("CLAUDE_CONFIG_DIR", claude_home)
        .env("CODEX_HOME", workspace.join("no-codex-home"))
        .env("ARSY_MCP_CLI_SECRET", "never-printed-7f3a")
        .args(["--output", "json"])
        .args(args)
        .output()
        .expect("the binary runs");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("never-printed-7f3a")
            && !String::from_utf8_lossy(&output.stderr).contains("never-printed-7f3a"),
        "a launch value reached the output of {args:?}"
    );
    let stdout = String::from_utf8(output.stdout).expect("machine output is UTF-8");
    let record = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["type"] == "result")
        .unwrap_or_else(|| {
            // Without the exit code and stderr, a run that produced nothing at
            // all reports an empty string and says nothing about why.
            panic!(
                "no result record for {args:?}\n  status: {:?}\n  stdout: {stdout}\n  stderr: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            )
        });
    (
        output.status.code().unwrap_or(-1),
        record["payload"].clone(),
    )
}

#[test]
fn a_connection_is_defined_listed_probed_disabled_and_removed() {
    let workspace = tempfile::tempdir().unwrap();
    // The server runs in its own directory so the probe cannot see, or be
    // confused by, the connection definition it is being reached through.
    let served = tempfile::tempdir().unwrap();
    let program = server_command();
    let scoped = ["--scope", "workspace"];

    let (code, added) = arsy(
        workspace.path(),
        &[
            &["mcp", "add", "fixture", "--command", program.as_str()][..],
            &scoped[..],
            &[
                "--",
                "--workspace",
                served.path().to_str().unwrap(),
                "serve",
            ][..],
        ]
        .concat(),
    );
    assert_eq!(code, 0, "{added}");
    assert_eq!(added["transport"], "stdio");
    assert_eq!(
        added["trust"], "workspace",
        "a repository file is untrusted"
    );
    assert_eq!(added["connected"], false, "defining is not connecting");

    // The definition appears in the listing without anything being connected.
    let (code, listed) = arsy(workspace.path(), &["mcp", "list", "--source", "arsy"]);
    assert_eq!(code, 0);
    let entries = listed["entries"].as_array().unwrap();
    // The bundled connection is always declared, so the listing carries it
    // beside whatever the operator defined.
    let fixture = entries
        .iter()
        .find(|entry| entry["name"] == "fixture")
        .unwrap_or_else(|| panic!("the definition is missing from {listed}"));
    assert_eq!(fixture["enabled"], true);
    assert_eq!(fixture["runtime_status"], "not_loaded");
    assert!(entries.iter().any(|entry| entry["name"] == "fluxguard"));

    // Adding the same name twice is refused rather than silently duplicated.
    let (code, _) = arsy(
        workspace.path(),
        &[
            &["mcp", "add", "fixture", "--command", program.as_str()][..],
            &scoped[..],
        ]
        .concat(),
    );
    assert_eq!(code, 2);

    let (code, probed) = arsy(workspace.path(), &["mcp", "test", "fixture"]);
    assert_eq!(code, 0, "{probed}");
    assert_eq!(probed["server"]["name"], "arsy");
    assert_eq!(probed["server"]["protocol_version"], "2026-07-28");
    let discovered: Vec<&str> = probed["discovery"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(discovered.contains(&"process.exec"), "{discovered:?}");
    assert!(discovered.contains(&"git.status"), "{discovered:?}");
    // The ceiling a real connection would hold is exactly what was discovered.
    assert_eq!(
        probed["would_admit"]["tools"].as_array().unwrap().len(),
        discovered.len()
    );
    assert_eq!(probed["tool_invoked"], false, "a probe never calls a tool");

    // Disabling leaves the definition in place and stops it connecting.
    let (code, disabled) = arsy(
        workspace.path(),
        &[&["mcp", "disable", "fixture"][..], &scoped[..]].concat(),
    );
    assert_eq!(code, 0);
    assert_eq!(disabled["enabled"], false);
    let (code, _) = arsy(workspace.path(), &["mcp", "test", "fixture"]);
    assert_eq!(code, 3, "a disabled connection is a policy refusal");
    let (_, listed) = arsy(workspace.path(), &["mcp", "list", "--source", "arsy"]);
    assert_eq!(listed["entries"][0]["enabled"], false);

    let (code, enabled) = arsy(
        workspace.path(),
        &[&["mcp", "enable", "fixture"][..], &scoped[..]].concat(),
    );
    assert_eq!(code, 0);
    assert_eq!(enabled["enabled"], true);

    let (code, removed) = arsy(
        workspace.path(),
        &[&["mcp", "remove", "fixture"][..], &scoped[..]].concat(),
    );
    assert_eq!(code, 0);
    assert_eq!(removed["removed"], true);
    let (_, listed) = arsy(workspace.path(), &["mcp", "list", "--source", "arsy"]);
    assert!(listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["name"] != "fixture"));

    // Removing what is not there is an error, not a silent success.
    let (code, _) = arsy(
        workspace.path(),
        &[&["mcp", "remove", "fixture"][..], &scoped[..]].concat(),
    );
    assert_eq!(code, 2);
    let (code, _) = arsy(workspace.path(), &["mcp", "test", "fixture"]);
    assert_eq!(code, 2);
}

/// A server only Claude Code declares is connectable with no `arsy.json` at all.
#[test]
fn a_server_claude_code_declares_is_probed_without_adopting_it() {
    let workspace = tempfile::tempdir().unwrap();
    let served = tempfile::tempdir().unwrap();
    let claude = tempfile::tempdir().unwrap();
    let declared = serde_json::json!({"mcpServers": {"from-claude": {
        "command": server_command(),
        "args": ["--workspace", served.path().to_str().unwrap(), "serve"],
        "env": {"ARSY_PROBE_TOKEN": "${ARSY_MCP_CLI_SECRET}"}
    }}});
    std::fs::write(claude.path().join(".claude.json"), declared.to_string()).unwrap();

    let (code, probed) = arsy_with_claude(
        workspace.path(),
        claude.path(),
        &["mcp", "test", "from-claude"],
    );
    assert_eq!(code, 0, "{probed}");
    assert_eq!(probed["server"]["name"], "arsy");

    let (code, explained) = arsy_with_claude(
        workspace.path(),
        claude.path(),
        &["config", "explain", "mcp.server.from-claude"],
    );
    assert_eq!(code, 0, "{explained}");
    assert!(
        explained.to_string().contains(".claude.json"),
        "the listing names the file it came from: {explained}"
    );
}

/// Unix-gated: it needs a program that reads nothing and answers nothing, and
/// `sleep` is the portable-across-unix way to say that.
#[cfg(unix)]
#[test]
fn a_server_that_never_answers_fails_on_the_deadline() {
    let workspace = tempfile::tempdir().unwrap();

    let (code, _) = arsy(
        workspace.path(),
        &[
            "mcp",
            "add",
            "silent",
            "--command",
            "sh",
            "--timeout",
            "1",
            "--scope",
            "workspace",
            "--",
            "-c",
            "sleep 30",
        ],
    );
    assert_eq!(code, 0);

    let started = std::time::Instant::now();
    let (code, _) = arsy(workspace.path(), &["mcp", "test", "silent"]);
    assert_eq!(code, 5, "a transport failure exits in the protocol class");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "the deadline must bound the probe, not the server's own lifetime"
    );
}

/// The bundled connection is declared before any file is read, so an operator
/// who wants it off has nothing in their config to edit. Disabling has to work
/// anyway, and has to leave behind a file the loader still accepts.
#[test]
fn the_bundled_connection_can_be_turned_off_without_restating_it() {
    let workspace = tempfile::tempdir().unwrap();
    let (code, disabled) = arsy(
        workspace.path(),
        &["mcp", "disable", "fluxguard", "--scope", "workspace"],
    );
    assert_eq!(code, 0, "{disabled}");
    assert_eq!(disabled["enabled"], false);

    // A name that is genuinely undefined is still refused: writing a
    // transport-less entry for one would only produce a file nothing can load.
    let (code, _) = arsy(
        workspace.path(),
        &["mcp", "disable", "absent", "--scope", "workspace"],
    );
    assert_ne!(code, 0);

    // The toggle alone has to survive a reload, which is what proves the
    // amendment path and the writer agree on the shape.
    let (code, listed) = arsy(workspace.path(), &["mcp", "list"]);
    assert_eq!(code, 0, "{listed}");
    let fluxguard = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "fluxguard")
        .expect("fluxguard is listed");
    assert_eq!(fluxguard["enabled"], false);
}
