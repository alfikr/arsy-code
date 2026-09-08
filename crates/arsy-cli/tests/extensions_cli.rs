//! `arsy plugin` and `arsy skill list` end to end.
//!
//! The interesting property is the one an operator relies on: a plugin that
//! grows a capability after install is held back until it is approved again,
//! and `refresh` reports it rather than adopting it.

use serde_json::Value;
use std::{path::Path, process::Command};

const MANIFEST: &str = r#"manifest_version = 1
id = "example.review"
version = "1.2.0"
entrypoint = "plugin.wasm"
api = ">=1,<2"
capabilities = ["fs.read:workspace/**"]
"#;

fn arsy(workspace: &Path, args: &[&str]) -> (i32, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_arsy"))
        .args(["--workspace", workspace.to_str().unwrap()])
        .args(["--output", "json"])
        .args(args)
        .output()
        .expect("the binary runs");
    let stdout = String::from_utf8(output.stdout).expect("machine output is UTF-8");
    let record = stdout
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
fn a_plugin_is_approved_installed_inspected_held_back_and_removed() {
    let workspace = tempfile::tempdir().unwrap();
    let source = workspace.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("plugin.toml"), MANIFEST).unwrap();
    std::fs::write(source.join("plugin.wasm"), b"\0asm\x01\0\0\0").unwrap();
    let source = source.to_str().unwrap();

    // Nothing is installed, and nothing was invented to fill the listing.
    let (code, listed) = arsy(workspace.path(), &["plugin", "list"]);
    assert_eq!(code, 0);
    assert!(listed["plugins"].as_array().unwrap().is_empty());

    // Without a terminal and without --force, an install stops rather than
    // granting a capability nobody agreed to.
    let (code, _) = arsy(workspace.path(), &["plugin", "install", source]);
    assert_eq!(code, 3, "an unconfirmed install is a policy refusal");
    let (_, listed) = arsy(workspace.path(), &["plugin", "list"]);
    assert!(listed["plugins"].as_array().unwrap().is_empty());

    let (code, installed) = arsy(workspace.path(), &["plugin", "install", source, "--force"]);
    assert_eq!(code, 0, "{installed}");
    assert_eq!(installed["installed"], true);
    assert_eq!(
        installed["granted"],
        serde_json::json!(["fs.read:workspace/**"])
    );

    let (code, inspected) = arsy(workspace.path(), &["plugin", "inspect", "example.review"]);
    assert_eq!(code, 0);
    assert_eq!(inspected["version"], "1.2.0");
    assert_eq!(inspected["loadable"], true);
    assert_eq!(inspected["signature"], "absent");
    assert!(inspected["beyond_grant"].as_array().unwrap().is_empty());
    assert!(inspected["limits"]["fuel"].as_u64().unwrap() > 0);

    let (code, listed) = arsy(workspace.path(), &["plugin", "list", "--capabilities"]);
    assert_eq!(code, 0);
    assert_eq!(
        listed["plugins"][0]["requested"],
        serde_json::json!(["fs.read:workspace/**"])
    );

    // Installing copies the files; a refresh is what puts them in force.
    let (code, adopted) = arsy(workspace.path(), &["plugin", "refresh"]);
    assert_eq!(code, 0);
    assert_eq!(adopted["added"], serde_json::json!(["example.review"]));
    assert_eq!(adopted["loaded"][0]["version"], "1.2.0");

    // The installed manifest grows a capability. Refresh must report it, not
    // adopt it: otherwise refresh would be the way around approval.
    let installed_manifest = workspace
        .path()
        .join(".arsy/plugins/example.review/plugin.toml");
    std::fs::write(
        &installed_manifest,
        MANIFEST.replace(
            "capabilities = [\"fs.read:workspace/**\"]",
            "capabilities = [\"fs.read:workspace/**\", \"process.exec:**\"]",
        ),
    )
    .unwrap();
    let (code, refreshed) = arsy(workspace.path(), &["plugin", "refresh"]);
    assert_eq!(code, 0);
    assert!(refreshed["added"].as_array().unwrap().is_empty());
    assert!(refreshed["updated"].as_array().unwrap().is_empty());
    assert!(refreshed["rejected"]["example.review"]
        .as_str()
        .unwrap()
        .contains("process.exec:**"));
    assert_eq!(
        refreshed["loaded"][0]["version"], "1.2.0",
        "a plugin beyond its grant keeps the version already in force"
    );
    let (_, listed) = arsy(workspace.path(), &["plugin", "list"]);
    assert_eq!(listed["plugins"][0]["loadable"], false);

    // Back within the grant, a version bump is adopted; --dry-run reports the
    // same plan without committing it.
    std::fs::write(&installed_manifest, MANIFEST.replace("1.2.0", "1.3.0")).unwrap();
    let (code, planned) = arsy(workspace.path(), &["plugin", "refresh", "--dry-run"]);
    assert_eq!(code, 0);
    assert_eq!(planned["dry_run"], true);
    assert_eq!(planned["committed"], false);
    assert_eq!(
        planned["updated"],
        serde_json::json!(["example.review"]),
        "a dry run still says what would change"
    );

    let (code, refreshed) = arsy(workspace.path(), &["plugin", "refresh", "example.review"]);
    assert_eq!(code, 0);
    assert_eq!(refreshed["committed"], true);
    assert_eq!(refreshed["loaded"][0]["version"], "1.3.0");

    let (code, removed) = arsy(workspace.path(), &["plugin", "remove", "example.review"]);
    assert_eq!(code, 0);
    assert_eq!(
        removed["revoked"],
        serde_json::json!(["fs.read:workspace/**"]),
        "removing revokes the grant, so a reinstall asks again"
    );
    let (code, _) = arsy(workspace.path(), &["plugin", "inspect", "example.review"]);
    assert_eq!(code, 2);
}

#[test]
fn skills_are_listed_as_data_and_hooks_carry_their_engine_semantics() {
    let workspace = tempfile::tempdir().unwrap();
    let skill = workspace.path().join(".claude/skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "# review\n").unwrap();
    std::fs::write(
        workspace.path().join(".claude/settings.json"),
        r#"{"hooks": {"PreToolUse": [{"matcher": "Bash",
             "hooks": [{"type": "command", "command": "check.sh"}]}],
             "Stop": [{"hooks": [{"type": "command", "command": "done.sh"}]}]}}"#,
    )
    .unwrap();

    let (code, listed) = arsy(workspace.path(), &["skill", "list"]);
    assert_eq!(code, 0);
    let skills = listed["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["name"], "review");
    assert_eq!(skills[0]["ecosystem"], "claude");
    assert_eq!(skills[0]["authority"], "data_only");

    let (code, hooks) = arsy(workspace.path(), &["hook", "list"]);
    assert_eq!(code, 0);
    let entries = hooks["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let gate = entries
        .iter()
        .find(|entry| entry["event"] == "before_operation")
        .expect("PreToolUse maps to before_operation");
    assert_eq!(gate["effect_class"], "gate");
    assert_eq!(gate["on_failure"], "fail_closed");
    assert_eq!(gate["runtime_status"], "not_loaded");
    let report = entries
        .iter()
        .find(|entry| entry["event"] == "after_turn")
        .expect("Stop maps to after_turn");
    assert_eq!(report["effect_class"], "observe");
    assert_eq!(report["on_failure"], "fail_open");
}
