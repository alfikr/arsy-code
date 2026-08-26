#!/usr/bin/env python3
import json
from pathlib import Path

ROOT = Path(__file__).parent
FIXTURES = {"agents", "claude", "codex", "omp"}
REQUIRED_COVERAGE = {
    "nested_instructions",
    "settings_precedence",
    "skills",
    "hooks",
    "agents",
    "plugins",
    "mcp",
}
LEVELS = {"parsed", "mapped", "behavior-tested", "unsupported"}


def require(condition, message):
    if not condition:
        raise SystemExit(message)


coverage = set()
for name in sorted(FIXTURES):
    fixture = ROOT / name
    require((fixture / "README.md").is_file(), f"{name}: missing README.md")
    canonical = json.loads((fixture / "expected/canonical.json").read_text())
    loss = json.loads((fixture / "expected/loss.json").read_text())
    require(canonical["schema_version"] == 1, f"{name}: canonical schema")
    require(loss["schema_version"] == 1, f"{name}: loss schema")
    require(canonical["fixture"] == name == loss["fixture"], f"{name}: fixture ID")
    coverage.update(canonical["coverage"])
    for section in ("instructions", "skills", "hooks", "agents", "plugins", "mcp"):
        for item in canonical[section]:
            require((fixture / item["source"]).is_file(), f"{name}: missing {item['source']}")
            require(item["level"] in LEVELS, f"{name}: invalid level")
    for item in loss["losses"]:
        require((fixture / item["source"]).is_file(), f"{name}: missing loss source")
        require(item["level"] in LEVELS, f"{name}: invalid loss level")

require(REQUIRED_COVERAGE <= coverage, f"missing coverage: {REQUIRED_COVERAGE - coverage}")
print(f"compat fixtures valid: {len(FIXTURES)}; coverage: {len(REQUIRED_COVERAGE)}/7")
