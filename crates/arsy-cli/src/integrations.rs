//! Inspection only: imported content is never an execution grant.
use crate::{usage, Command, Diagnostic};
use arsy_code::compat::{CompatibilityImporter, Ecosystem};
use serde_json::{json, Value};
use std::path::Path;

pub fn parse(
    kind: &str,
    args: Vec<String>,
    source: Option<String>,
    event: Option<String>,
) -> Result<Command, Diagnostic> {
    if source
        .as_deref()
        .is_some_and(|source| !matches!(source, "arsy" | "claude" | "codex" | "omp"))
    {
        return Err(usage("--source requires arsy, claude, codex, or omp"));
    }
    if kind == "mcp" && event.is_some() {
        return Err(usage("--event applies only to hook list"));
    }
    if kind == "hook" && source.as_deref().is_some_and(|source| source != "claude") {
        return Err(usage(
            "hook inspection currently supports --source claude only",
        ));
    }
    let name = match args.as_slice() {
        [action] if action == "list" => None,
        [action, name] if kind == "mcp" && action == "show" => Some(name.clone()),
        _ => {
            return Err(usage(if kind == "mcp" {
                "mcp requires `list` or `show <NAME>`; --source claude|codex|omp. Connection management is not implemented."
            } else {
                "hook requires `list`; --event <NAME>, --source claude"
            }))
        }
    };
    Ok(Command::Inspect {
        kind: kind.into(),
        name,
        source,
        event,
    })
}

pub fn inspect(
    root: &Path,
    kind: &str,
    name: Option<&str>,
    source: Option<&str>,
    event: Option<&str>,
) -> Result<Value, Diagnostic> {
    let cwd = std::env::current_dir().map_err(crate::storage_failed)?;
    let working = if cwd.starts_with(root) {
        cwd.as_path()
    } else {
        root
    };
    let importer = CompatibilityImporter::new(root);
    let mut entries = Vec::new();
    // What the engine actually built, so a listing can say which declarations
    // run rather than repeating that none do.
    let loaded = if kind == "hook" {
        Some(crate::hook_engine(
            root,
            &crate::load_config(root, working)?,
        ))
    } else {
        None
    };
    if kind == "mcp" && source.is_none_or(|source| source == "arsy") {
        entries.extend(configured(root, working, name)?);
    }
    for ecosystem in [Ecosystem::Claude, Ecosystem::Codex, Ecosystem::Omp] {
        if source.is_some_and(|source| source != ecosystem.as_str()) {
            continue;
        }
        if kind == "hook" && ecosystem != Ecosystem::Claude {
            continue;
        }
        let declarations = if kind == "mcp" {
            importer.mcp_declarations(ecosystem, working)
        } else {
            importer.hook_declarations()
        }
        .map_err(|error| {
            Diagnostic::error(
                "ARSY-CMP-1001",
                format!("{} import failed: {error}", ecosystem.as_str()),
                "fix the source configuration; no connection or hook was executed",
            )
        })?;
        for mut entry in declarations {
            if name.is_some_and(|name| entry["name"].as_str() != Some(name)) {
                continue;
            }
            if event.is_some_and(|event| {
                entry["event"].as_str() != Some(event)
                    && entry["original_event"].as_str() != Some(event)
            }) {
                continue;
            }
            entry["ecosystem"] = json!(ecosystem.as_str());
            entry["runtime_status"] = json!(match &loaded {
                Some(loaded) => runtime_status(loaded, root, entry["source"].as_str()),
                None => "not_loaded",
            });
            if kind == "hook" {
                annotate_hook(&mut entry);
            }
            entries.push(entry);
        }
    }
    if name.is_some() && entries.is_empty() {
        return Err(usage(format!(
            "no MCP connection or declaration is named `{}`; list the available names with `arsy mcp list` (`/mcp` in the TUI)",
            name.unwrap_or_default()
        )));
    }
    if name.is_some() && entries.len() > 1 {
        return Err(usage(format!(
            "`{}` is declared by {} sources; select one with --source arsy|claude|codex|omp",
            name.unwrap_or_default(),
            entries.len()
        )));
    }
    let mut report = json!({
        "entries": entries,
        "status": "inspection_complete",
        "notice": match &loaded {
            // A hook listing is no longer only a reading of files: some of what
            // it names will run, and saying otherwise would be false.
            Some(loaded) if !loaded.is_empty() => "Hooks marked `loaded` run on this workspace's turns. A repository's own hooks run only where `[project.\"<path>\"] trust_level = \"trusted\"` vouches for it.",
            Some(_) => "No executable hook is loaded for this workspace. Provider-owned integrations are managed by the provider.",
            None => "Definitions and declarations only; ARSY has not connected. Provider-owned integrations are managed by the provider.",
        },
    });
    if let Some(loaded) = loaded {
        // Where the engine looked, including the operator's own files, which
        // an import of this workspace never sees.
        report["sources"] = json!(loaded.sources);
    }
    Ok(report)
}

/// Whether the file a declaration came from is one the engine loaded.
///
/// Matched on the whole path rather than its tail: the operator's own
/// `~/.claude/settings.json` and the repository's end in the same characters,
/// and taking the first of those to match reported one file's status against
/// the other's declarations.
fn runtime_status(
    loaded: &arsy_code::hook::Loaded,
    root: &Path,
    source: Option<&str>,
) -> &'static str {
    let Some(source) = source else {
        return "not_loaded";
    };
    let declared = root.join(source);
    let same = |candidate: &Path| {
        candidate == declared
            || std::fs::canonicalize(candidate).ok() == std::fs::canonicalize(&declared).ok()
    };
    loaded
        .sources
        .iter()
        .find(|candidate| same(&candidate.path))
        .map_or("not_loaded", |candidate| {
            if candidate.status == "loaded" {
                "loaded"
            } else {
                "not_loaded"
            }
        })
}

/// ARSY's own `[mcp.server.*]` connections, in the same row shape the imported
/// declarations use so one listing can show both.
///
/// Reading a definition is not connecting: `runtime_status` is `not_loaded`
/// for every row here, exactly as it is for an import.
fn configured(root: &Path, working: &Path, name: Option<&str>) -> Result<Vec<Value>, Diagnostic> {
    let config = crate::load_config(root, working)?;
    let mut entries = Vec::new();
    for server in config.mcp_servers() {
        if name.is_some_and(|name| name != server.name) {
            continue;
        }
        let mut entry = json!({
            "name": server.name,
            "source": format!("{} ({})", arsy_kernel::config::CONFIG_FILE, server.trust),
            "ecosystem": "arsy",
            "transport": server.transport.kind(),
            "trust": server.trust.to_string(),
            "level": "native",
            "enabled": server.enabled,
            "runtime_status": "not_loaded",
            "timeout_ms": server.timeout_ms,
            "max_body_bytes": server.max_body_bytes,
        });
        match &server.transport {
            arsy_kernel::config::McpTransport::Stdio { command, args } => {
                entry["command"] = json!(command);
                entry["args"] = json!(args);
            }
            arsy_kernel::config::McpTransport::Http { url } => entry["url"] = json!(url),
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// Add what the lifecycle engine would make of a declared hook: whether the
/// event exists in this build, what it may do, and what happens if it fails.
///
/// A declaration says what its author intended; these three say what ARSY would
/// actually do with it, which is the difference `arsy hook list` has to show.
fn annotate_hook(entry: &mut Value) {
    use arsy_code::hook::{EffectClass, LifecycleEvent};

    let Some(event) = entry["event"].as_str().and_then(LifecycleEvent::parse) else {
        entry["effect_class"] = json!("none");
        entry["on_failure"] = json!("not_dispatched");
        return;
    };
    // An imported hook is repository content: it may observe and it may deny,
    // but it is registered as a gate only where the event is one that gates.
    entry["effect_class"] = serde_json::to_value(match event.failure_policy() {
        arsy_code::hook::FailurePolicy::FailClosed => EffectClass::Gate,
        arsy_code::hook::FailurePolicy::FailOpen => EffectClass::Observe,
    })
    .unwrap_or(Value::Null);
    entry["on_failure"] = serde_json::to_value(event.failure_policy()).unwrap_or(Value::Null);
}

/// Render the inspection for a person instead of echoing the machine record.
///
/// Each row keeps the fields that decide whether a declaration is the one its
/// author intended — the command or URL that a connection *would* run, the
/// trust label, and the mapping level. The full record stays available under
/// `--output json`, which is what a machine reads.
pub fn human_report(
    report: &Value,
    kind: &str,
    source: Option<&str>,
    event: Option<&str>,
) -> Value {
    let entries = report["entries"]
        .as_array()
        .expect("inspection produces an entries array");
    if entries.is_empty() {
        return json!({
            "declarations": nothing_found(kind, source, event),
            "notice": report["notice"],
        });
    }
    let mut listing = summary(entries, kind);
    for entry in entries {
        let label = entry["name"]
            .as_str()
            .or_else(|| entry["original_event"].as_str())
            .unwrap_or("hook");
        let detail = entry["transport"]
            .as_str()
            .or_else(|| entry["matcher"].as_str())
            .unwrap_or("");
        let status = if entry["runtime_status"] == "loaded" {
            "loaded"
        } else {
            "not loaded"
        };
        listing.push_str(&format!("\n  {label} · {detail} · {status}\n"));
        for line in details(kind, entry) {
            listing.push_str(&format!("    {line}\n"));
        }
    }
    json!({"declarations": listing, "notice": report["notice"]})
}

/// The count line, so a long listing states up front how much of it is usable.
fn summary(entries: &[Value], kind: &str) -> String {
    let noun = if kind == "mcp" { "MCP server" } else { "hook" };
    let plural = if entries.len() == 1 { "" } else { "s" };
    let unsupported = entries
        .iter()
        .filter(|entry| entry["level"] == "unsupported")
        .count();
    let mut summary = format!("{} {noun}{plural} declared", entries.len());
    if unsupported > 0 {
        summary.push_str(&format!(", {unsupported} unsupported"));
    }
    summary.push_str("; none loaded\n");
    summary
}

fn details(kind: &str, entry: &Value) -> Vec<String> {
    let text = |key: &str| entry[key].as_str().unwrap_or("unknown");
    let mut rows = vec![format!(
        "source: {} ({})",
        text("source"),
        text("ecosystem")
    )];
    if kind == "mcp" {
        rows.push(match entry["command"].as_str() {
            Some(command) => {
                let args = entry["args"]
                    .as_array()
                    .map(|args| {
                        args.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                format!("command: {command} {args}").trim_end().to_owned()
            }
            None => format!("url: {}", text("url")),
        });
        let mut state = format!("trust: {} · level: {}", text("trust"), text("level"));
        if entry["enabled"] == Value::Bool(false) {
            state.push_str(" · disabled by its source");
        }
        rows.push(state);
    } else {
        rows.push(format!("lifecycle: {}", text("event")));
        let handlers = entry["handlers"]
            .as_array()
            .map(|handlers| {
                handlers
                    .iter()
                    .filter_map(|handler| handler["type"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        // A declared event can carry an empty handler list; a blank field reads
        // as a rendering fault rather than as the hook doing nothing.
        let handlers = if handlers.is_empty() {
            "none".to_owned()
        } else {
            handlers
        };
        rows.push(format!(
            "handlers: {handlers} · effect: {} · level: {}",
            text("effect"),
            text("level")
        ));
        rows.push(format!(
            "engine: {} · on failure: {}",
            text("effect_class"),
            text("on_failure")
        ));
    }
    rows
}

/// An empty result is ambiguous on its own, so it names what was read and which
/// filters were applied — otherwise a typo in `--event` looks like a missing
/// integration.
fn nothing_found(kind: &str, source: Option<&str>, event: Option<&str>) -> String {
    // `--source` narrows what `inspect` reads, so the list has to narrow with
    // it: naming a file that was skipped sends the reader to the wrong place.
    let searched = if kind == "mcp" {
        [
            (".mcp.json (claude)", "claude"),
            (".codex/config.toml (codex)", "codex"),
            ("the nearest .omp/mcp.json (omp)", "omp"),
        ]
        .into_iter()
        .filter(|(_, ecosystem)| source.is_none_or(|source| source == *ecosystem))
        .map(|(file, _)| file)
        .collect::<Vec<_>>()
        .join(", ")
    } else {
        ".claude/settings.json, .claude/settings.local.json".to_owned()
    };
    let filters: Vec<String> = [
        source.map(|s| format!("--source {s}")),
        event.map(|e| format!("--event {e}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut message = format!("No declarations matched; searched {searched}.");
    if !filters.is_empty() {
        message.push_str(&format!(" Filters applied: {}.", filters.join(", ")));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_commands_validate_arguments_and_read_real_fixtures() {
        for args in [
            vec!["mcp", "list"],
            vec!["mcp", "show", "x", "--source", "codex"],
            vec!["hook", "list", "--event", "PreToolUse"],
        ] {
            assert!(crate::parse(args.into_iter().map(str::to_owned)).is_ok());
        }
        for args in [
            vec!["mcp"],
            vec!["hook", "run"],
            vec!["mcp", "list", "--event", "Stop"],
            vec!["hook", "list", "--source", "invalid"],
        ] {
            assert!(crate::parse(args.into_iter().map(str::to_owned)).is_err());
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/compat/claude/input")
            .canonicalize()
            .unwrap();
        let hooks = inspect(&root, "hook", None, Some("claude"), Some("PreToolUse")).unwrap();
        assert!(!hooks["entries"].as_array().unwrap().is_empty());
        assert_eq!(hooks["entries"][0]["runtime_status"], "not_loaded");
        let mcp = inspect(&root, "mcp", None, Some("claude"), None).unwrap();
        assert!(!mcp["entries"].as_array().unwrap().is_empty());
        assert!(inspect(&root, "mcp", Some("missing"), Some("claude"), None).is_err());
        assert_eq!(
            inspect(&root, "mcp", None, None, None).unwrap()["entries"],
            mcp["entries"]
        );
        let listing = human_report(&mcp, "mcp", Some("claude"), None)["declarations"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(listing.contains("not loaded"), "{listing}");
        assert!(listing.contains("declared; none loaded"), "{listing}");
        // The record itself is never printed at a person: only the fields that
        // say what a connection would run.
        assert!(!listing.contains("\"runtime_status\""), "{listing}");
        assert!(
            listing.contains("command: ") || listing.contains("url: "),
            "{listing}"
        );

        // An empty result names what was read and which filters narrowed it.
        let empty = inspect(&root, "hook", None, Some("claude"), Some("NoSuchEvent")).unwrap();
        let empty = human_report(&empty, "hook", Some("claude"), Some("NoSuchEvent"))
            ["declarations"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(empty.contains(".claude/settings.json"), "{empty}");
        assert!(empty.contains("--event NoSuchEvent"), "{empty}");

        // `--source` skips the other ecosystems, so naming their files would
        // send the reader to a file that was never read.
        let scoped = human_report(
            &json!({"entries": [], "notice": ""}),
            "mcp",
            Some("codex"),
            None,
        )["declarations"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(scoped.contains(".codex/config.toml"), "{scoped}");
        assert!(!scoped.contains(".mcp.json"), "{scoped}");
        assert!(!scoped.contains(".omp/mcp.json"), "{scoped}");
    }

    #[test]
    fn hook_rows_report_lifecycle_handlers_and_unsupported_events() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/compat/claude/input")
            .canonicalize()
            .unwrap();
        let hooks = inspect(&root, "hook", None, Some("claude"), None).unwrap();
        let listing = human_report(&hooks, "hook", None, None)["declarations"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(listing.contains("lifecycle: "), "{listing}");
        assert!(listing.contains("handlers: "), "{listing}");
        assert!(listing.contains("effect: "), "{listing}");
    }
}
