//! Claude Code and Codex settings, read live by the real binary.
//!
//! Each test gives the binary a Claude and a Codex home of its own, so what it
//! asserts never depends on the setup of the machine running it.

use serde_json::Value;
use std::{path::Path, process::Command};

fn arsy(workspace: &Path, homes: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_arsy"))
        .args(["--workspace", workspace.to_str().unwrap()])
        .env("CLAUDE_CONFIG_DIR", homes.join("claude"))
        .env("CODEX_HOME", homes.join("codex"))
        .args(["--output", "json"])
        .args(args)
        .output()
        .expect("the binary runs");
    let stdout = String::from_utf8(output.stdout).expect("machine output is UTF-8");
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["type"] == "result")
        .unwrap_or_else(|| {
            panic!(
                "no result for {args:?}\n  stdout: {stdout}\n  stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        })["payload"]
        .clone()
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[test]
fn claude_permissions_decide_what_a_call_may_do() {
    let workspace = tempfile::tempdir().unwrap();
    let homes = tempfile::tempdir().unwrap();
    write(
        &homes.path().join("claude/settings.json"),
        r#"{"permissions": {"allow": ["Read(src/**)", "Bash(cargo:*)"], "deny": ["Bash(rm:*)"]}}"#,
    );
    let decide = |action: &str, resource: &str| {
        arsy(
            workspace.path(),
            homes.path(),
            &["policy", "explain", action, "--resource", resource],
        )["decision"]
            .clone()
    };
    let decision = |program: &str| decide("process.exec", &format!("process:{program}"));
    assert_eq!(decision("rm"), "deny");
    assert_eq!(decide("fs.read", "file:src/main.rs"), "allow");
    // ARSY never waves irreversible work through on a rule alone, whoever
    // wrote the rule, so an allowed program is still confirmed.
    assert_eq!(decision("cargo"), "ask");

    // Switched off, Claude's rules no longer apply.
    write(
        &workspace.path().join(".arsy/arsy.json"),
        r#"{"schema_version": 1, "compat": {"claude": {"enabled": false}}}"#,
    );
    assert_ne!(decision("rm"), "deny");
}

#[test]
fn a_read_only_codex_sandbox_asks_before_writing() {
    let workspace = tempfile::tempdir().unwrap();
    let homes = tempfile::tempdir().unwrap();
    write(
        &homes.path().join("codex/config.toml"),
        "sandbox_mode = \"read-only\"\n",
    );
    let explained = arsy(
        workspace.path(),
        homes.path(),
        &[
            "policy",
            "explain",
            "fs.write",
            "--resource",
            "file:notes.txt",
        ],
    );
    assert_eq!(explained["decision"], "ask", "{explained}");
    // The rule is Codex's, not only the built-in default that also asks.
    assert_eq!(explained["configured_rules"], 2, "{explained}");
}
