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
        // Never the Claude Code or Codex setup of the machine running the test.
        .env("CLAUDE_CONFIG_DIR", workspace.join("no-claude-home"))
        .env("CODEX_HOME", workspace.join("no-codex-home"))
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

    // A named revision reviews a branch rather than the working tree: against
    // the first commit, the committed file is itself the change.
    git(root, &["add", "."]);
    git(root, &["commit", "--quiet", "-m", "second"]);
    let (code, since) = arsy(root, &["review", "HEAD~1"]);
    assert_eq!(code, 0);
    assert_eq!(since["base"], "HEAD~1");
    assert_eq!(since["files"].as_array().unwrap().len(), 1);
    // And a revision this repository does not have is refused rather than
    // reported as an empty change.
    let (code, _) = arsy(root, &["review", "no-such-branch"]);
    assert_eq!(code, 7);
}

/// A change bigger than the byte cap is still a review.
///
/// The cap was applied by dropping the pipe, so git wrote into a closed one,
/// died of SIGPIPE, and the non-zero status was reported as the base revision
/// not existing — a remediation about naming a revision, for a repository
/// where the revision was fine.
#[test]
fn a_diff_past_the_cap_is_still_read_rather_than_blamed_on_the_revision() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    git(root, &["init", "--quiet"]);
    std::fs::write(root.join("seed.txt"), "seed\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "--quiet", "-m", "first"]);

    // Comfortably past the 8 MiB cap, and non-ASCII so the cut lands inside a
    // character rather than neatly between two.
    let line = "café ".repeat(200);
    std::fs::write(root.join("big.txt"), format!("{line}\n").repeat(12_000)).unwrap();
    // Staged, because `git diff HEAD` is what a review reads and an untracked
    // file is not in it.
    git(root, &["add", "big.txt"]);

    let (code, review) = arsy(root, &["review"]);

    assert_eq!(code, 0, "{review}");
    let files = review["files"].as_array().unwrap();
    assert!(
        files.iter().any(|file| file["path"] == "big.txt"),
        "{review}"
    );
}
