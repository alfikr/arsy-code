use arsy_code::compat::{CompatibilityImporter, Ecosystem};
use serde_json::Value;
use std::{fs, path::Path};

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
    assert_eq!(actual.canonical, canonical, "{name} canonical mapping");
    assert_eq!(actual.loss, loss, "{name} loss report");
}

#[test]
fn every_compatibility_fixture_matches_canonical_output_and_loss() {
    fixture("agents", Ecosystem::AgentsMd, "services/payments");
    fixture("claude", Ecosystem::Claude, "services/payments");
    fixture("codex", Ecosystem::Codex, "services/api");
    fixture("omp", Ecosystem::Omp, "packages/api");
}
