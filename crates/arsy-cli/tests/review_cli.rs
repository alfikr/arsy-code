//! `arsy review` against a real repository, because the change it reads comes
//! from Git and a parser tested only on canned text is a parser tested on the
//! author's idea of what Git prints.

use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Stdio},
};

fn git(repository: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repository)
        .env("GIT_AUTHOR_NAME", "tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.invalid")
        .env("GIT_COMMITTER_NAME", "tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.invalid")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

fn arsy(workspace: &Path, args: &[&str]) -> (i32, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_arsy"))
        .args(["--workspace", workspace.to_str().unwrap()])
        .args(args)
        .args(["--output", "json"])
        .output()
        .expect("the binary runs");
    let stdout = String::from_utf8(output.stdout).expect("machine output is UTF-8");
    let record: Value = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["type"] == "result")
        .unwrap_or_else(|| panic!("no result record in {stdout}"));
    (
        output.status.code().unwrap_or(-1),
        record["payload"].clone(),
    )
}

#[test]
fn a_working_tree_change_is_read_assessed_and_can_gate_a_pipeline() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    git(root, &["init", "--quiet"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/policy.rs"),
        "pub fn allow() -> bool {\n    true\n}\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "--quiet", "-m", "first"]);

    // A clean tree has nothing to review, and says so rather than inventing a
    // risk for a change that does not exist.
    let (code, clean) = arsy(root, &["review"]);
    assert_eq!(code, 0);
    assert!(clean["files"].as_array().unwrap().is_empty());
    assert!(clean["findings"].as_array().unwrap().is_empty());

    std::fs::write(
        root.join("src/policy.rs"),
        "pub fn allow(actor: &str) -> bool {\n    actor == \"root\"\n}\n",
    )
    .unwrap();

    let (code, review) = arsy(root, &["review"]);
    assert_eq!(code, 0);
    let files = review["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "src/policy.rs");
    assert_eq!(files[0]["removed_public_items"][0], "fn allow");

    let messages: Vec<&str> = review["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|finding| finding["kind"].as_str().unwrap())
        .collect();
    assert!(messages.contains(&"security"), "{messages:?}");
    assert!(messages.contains(&"compatibility"), "{messages:?}");
    assert!(messages.contains(&"verification"), "{messages:?}");

    // Nothing measured coverage, so the depth does not assume any.
    assert_eq!(review["depth"], "workspace");
    assert_eq!(review["risk"]["relevant_coverage_percent"], Value::Null);

    // The same review, as a gate.
    let (code, _) = arsy(root, &["review", "--strict"]);
    assert_eq!(code, 7);
}
