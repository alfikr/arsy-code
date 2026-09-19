use arsy_code::compat::{CompatibilityImporter, Ecosystem};
use serde_json::Value;
use std::{fs, path::Path};

/// The fixture's own location replaced, so a row that names the file a
/// declaration was read from does not depend on where the checkout lives.
/// The same substitution `arsy-compat`'s live golden makes.
fn relative(value: &Value, input: &Path) -> Value {
    let text = serde_json::to_string(value)
        .unwrap()
        .replace(&input.display().to_string(), "<fixture>")
        .replace('\\', "/");
    serde_json::from_str(&text).unwrap()
}

fn fixture(name: &str, ecosystem: Ecosystem, cwd: &str) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/compat")
        .join(name);
    let input = fixture.join("input");
    let actual = CompatibilityImporter::with_display_root(&input, &fixture)
        .import(ecosystem, &input.join(cwd), name)
        .unwrap();
    let canonical: Value =
        serde_json::from_slice(&fs::read(fixture.join("expected/canonical.json")).unwrap())
            .unwrap();
    let loss: Value =
        serde_json::from_slice(&fs::read(fixture.join("expected/loss.json")).unwrap()).unwrap();
    assert_eq!(
        relative(&actual.canonical, &input),
        relative(&canonical, &input),
        "{name} canonical mapping"
    );
    assert_eq!(
        relative(&actual.loss, &input),
        relative(&loss, &input),
        "{name} loss report"
    );
}

#[test]
fn every_compatibility_fixture_matches_canonical_output_and_loss() {
    fixture("agents", Ecosystem::AgentsMd, "services/payments");
    fixture("claude", Ecosystem::Claude, "services/payments");
    fixture("codex", Ecosystem::Codex, "services/api");
    fixture("omp", Ecosystem::Omp, "packages/api");
}
