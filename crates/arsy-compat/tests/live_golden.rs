//! `fixtures/compat/live`: everything Claude Code and Codex contribute, pinned.
//!
//! Set `ARSY_UPDATE_GOLDEN=1` to rewrite the expectation after an intended
//! change, then read the diff before committing it.

use arsy_compat::{instructions, seeds, CompatHomes, Context};
use arsy_kernel::config::{CompatSeed, McpTransport};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/compat/live")
}

/// The fixture's own location replaced, so the expectation does not depend on
/// where the checkout lives.
fn relative(text: &str, input: &Path) -> String {
    text.replace(&input.display().to_string(), "<fixture>")
}

/// A seed as data a reader can check.
fn describe(seed: &CompatSeed, input: &Path) -> Value {
    json!({
        "label": seed.label,
        "source": relative(&seed.path.display().to_string(), input),
        "mcp_servers": seed.mcp_servers.iter().map(|server| {
            let keys: Vec<&str> = match &server.transport {
                McpTransport::Stdio { env, .. } => env.keys().collect(),
                McpTransport::Http { headers, .. } => headers.keys().collect(),
            };
            json!({
                "name": server.name,
                "target": server.transport.target(),
                "enabled": server.enabled,
                "trust": server.trust.to_string(),
                "timeout_ms": server.timeout_ms,
                "launch_keys": keys,
            })
        }).collect::<Vec<_>>(),
        "policy_rules": seed.policy_rules.iter().map(|(id, rule)| json!({
            "id": id,
            "rule": format!("{:?} {} {} ({})", rule.effect, rule.action, rule.pattern, rule.source),
        })).collect::<Vec<_>>(),
        "models": seed.models.iter().map(|hint| json!({
            "model": hint.model,
            "dialects": format!("{:?}", hint.dialects),
            "provider": hint.provider,
        })).collect::<Vec<_>>(),
        "notes": seed.notes.iter().map(|note| relative(note, input)).collect::<Vec<_>>(),
    })
}

fn contributes(seed: &CompatSeed) -> bool {
    !(seed.mcp_servers.is_empty()
        && seed.policy_rules.is_empty()
        && seed.models.is_empty()
        && seed.notes.is_empty())
}

#[test]
fn the_live_view_of_a_claude_and_codex_setup_is_pinned() {
    let input = fixture().join("input").canonicalize().unwrap();
    let home = input.join("home");
    let homes = CompatHomes {
        claude_dir: Some(home.join(".claude")),
        claude_json: Some(home.join(".claude.json")),
        codex_dir: Some(home.join(".codex")),
    };
    let root = input.join("workspace");
    let resolved = seeds(&Context {
        homes: &homes,
        root: &root,
        trusted: false,
        claude: true,
        codex: true,
        env: &|name| (name == "DOCS_TOKEN").then(|| "fixture-secret-value".to_owned()),
    });
    let found = instructions::user_instructions(&homes, true, true, 64 * 1024);

    let actual = json!({
        "schema_version": 1,
        "fixture": "live",
        "seeds": resolved
            .iter()
            .filter(|seed| contributes(seed))
            .map(|seed| describe(seed, &input))
            .collect::<Vec<_>>(),
        "user_instructions": found
            .iter()
            .map(|instruction| relative(&instruction.path.display().to_string(), &input))
            .collect::<Vec<_>>(),
    });
    let rendered = serde_json::to_string_pretty(&actual).unwrap() + "\n";
    assert!(
        !rendered.contains("fixture-secret-value"),
        "a secret value reached the view"
    );

    let expected = fixture().join("expected/resolved.json");
    if std::env::var_os("ARSY_UPDATE_GOLDEN").is_some() {
        std::fs::write(&expected, &rendered).unwrap();
    }
    assert_eq!(
        rendered,
        std::fs::read_to_string(&expected).unwrap(),
        "the live view changed; rerun with ARSY_UPDATE_GOLDEN=1 and review the diff"
    );
}
