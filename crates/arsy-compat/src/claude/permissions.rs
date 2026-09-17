//! Claude Code's `permissions.allow`, `.ask`, and `.deny`.
//!
//! Read from the operator's `settings.json` and the repository's
//! `.claude/settings.json` and `settings.local.json`. Tools map as:
//!
//! - `Read(glob)` → `fs.read`; `Edit`, `Write`, `MultiEdit`, `NotebookEdit` →
//!   `fs.write`, over the path glob
//! - `WebFetch(domain:host)` → `network.connect` over the host
//! - `mcp__server` and `mcp__server__tool` → `mcp.invoke`
//! - `Bash(program:*)` or `Bash(program *)` → `process.exec` over the program
//!
//! Anything else is noted and left out.

use crate::{
    mcp::strings,
    permissions::{effect_name, rule},
    read, Context, Scope,
};
use arsy_kernel::{capability::CapabilityAction, config::CompatSeed, policy::RuleEffect};
use serde_json::Value;
use std::path::Path;

/// What one entry means to ARSY, and whether ARSY can hold it exactly.
struct Mapped {
    action: CapabilityAction,
    glob: String,
    exact: bool,
}

pub(crate) fn seeds(context: &Context) -> Vec<CompatSeed> {
    let user = context
        .homes
        .claude_dir
        .as_ref()
        .map(|directory| (directory.join("settings.json"), Scope::User));
    let project = [".claude/settings.json", ".claude/settings.local.json"]
        .map(|relative| Some((context.root.join(relative), Scope::Workspace)));
    std::iter::once(user)
        .chain(project)
        .flatten()
        .map(|(path, scope)| settings_seed(&path, scope))
        .collect()
}

fn settings_seed(path: &Path, scope: Scope) -> CompatSeed {
    let mut seed = CompatSeed {
        label: "claude".to_owned(),
        path: path.to_path_buf(),
        ..CompatSeed::default()
    };
    let settings = match read::json(path) {
        Ok(Some(settings)) => settings,
        Ok(None) => return seed,
        Err(error) => {
            seed.notes.push(format!("{} {error}", path.display()));
            return seed;
        }
    };
    let permissions = settings.get("permissions").cloned().unwrap_or(Value::Null);
    let entries = [
        ("deny", RuleEffect::Deny),
        ("ask", RuleEffect::RequireApproval),
        ("allow", RuleEffect::Allow),
    ]
    .into_iter()
    .flat_map(|(list, effect)| {
        strings(&permissions, list)
            .into_iter()
            .map(move |entry| (effect, entry))
    });
    for (effect, entry) in entries {
        place(&mut seed, scope, effect, &entry);
    }
    seed
}

fn place(seed: &mut CompatSeed, scope: Scope, declared: RuleEffect, entry: &str) {
    let Some(mapped) = map(entry) else {
        seed.notes
            .push(format!("`{entry}` has no ARSY equivalent; not applied"));
        return;
    };
    let effect = match (declared, mapped.exact) {
        (effect, true) => effect,
        (RuleEffect::Allow, false) | (RuleEffect::Deny, false) => {
            seed.notes.push(format!(
                "`{entry}` names more than ARSY matches on, so it asks before every `{}` instead",
                mapped.glob
            ));
            RuleEffect::RequireApproval
        }
        (effect, false) => effect,
    };
    let id = format!(
        "claude/{}/{}/{entry}",
        scope.as_str(),
        effect_name(declared)
    );
    match rule(scope, effect, mapped.action, &mapped.glob) {
        Some(rule) => seed.policy_rules.push((id, rule)),
        None => seed.notes.push(format!(
            "`{entry}` is not a pattern ARSY accepts; not applied"
        )),
    }
}

fn map(entry: &str) -> Option<Mapped> {
    let (tool, specifier) = match entry.split_once('(') {
        Some((tool, rest)) => (tool, Some(rest.strip_suffix(')')?)),
        None => (entry, None),
    };
    match tool {
        "Read" => file(CapabilityAction::FsRead, specifier),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            file(CapabilityAction::FsWrite, specifier)
        }
        "WebFetch" => host(specifier),
        "Bash" => process(specifier),
        tool => mcp(tool),
    }
}

fn file(action: CapabilityAction, specifier: Option<&str>) -> Option<Mapped> {
    let glob = match specifier {
        None => "**".to_owned(),
        // Home-relative paths depend on whose home, which a call's own path
        // argument never says.
        Some(path) if path.starts_with('~') => return None,
        Some(path) => match path.strip_prefix("//") {
            Some(absolute) => format!("/{absolute}"),
            None => path
                .trim_start_matches("./")
                .trim_start_matches('/')
                .to_owned(),
        },
    };
    Some(Mapped {
        action,
        glob,
        exact: true,
    })
}

fn host(specifier: Option<&str>) -> Option<Mapped> {
    let glob = match specifier {
        None => "**".to_owned(),
        Some(specifier) => specifier.strip_prefix("domain:")?.to_owned(),
    };
    Some(Mapped {
        action: CapabilityAction::NetworkConnect,
        glob,
        exact: true,
    })
}

/// ARSY matches a process on its program alone, so only a rule about a whole
/// program — `git:*`, `git *`, or bare `Bash` — is held exactly.
fn process(specifier: Option<&str>) -> Option<Mapped> {
    let Some(specifier) = specifier.map(str::trim) else {
        // `**`, so a program named by its absolute path is covered too.
        return Some(Mapped {
            action: CapabilityAction::ProcessExec,
            glob: "**".to_owned(),
            exact: true,
        });
    };
    let whole_program = specifier
        .strip_suffix(":*")
        .or_else(|| specifier.strip_suffix(" *"))
        .filter(|program| !program.contains(char::is_whitespace));
    let program = whole_program.or_else(|| specifier.split_whitespace().next())?;
    Some(Mapped {
        action: CapabilityAction::ProcessExec,
        glob: program.trim_end_matches(":*").to_owned(),
        exact: whole_program.is_some(),
    })
}

fn mcp(tool: &str) -> Option<Mapped> {
    let rest = tool.strip_prefix("mcp__")?;
    let glob = match rest.split_once("__") {
        Some((server, tool)) => format!("{server}/{tool}"),
        None => format!("{rest}/**"),
    };
    Some(Mapped {
        action: CapabilityAction::McpInvoke,
        glob,
        exact: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::policy::PolicyRule;

    fn rules(settings: &str, scope: Scope) -> (Vec<(String, PolicyRule)>, Vec<String>) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        std::fs::write(&path, settings).unwrap();
        let seed = settings_seed(&path, scope);
        (seed.policy_rules, seed.notes)
    }

    fn shown(rules: &[(String, PolicyRule)]) -> Vec<String> {
        rules
            .iter()
            .map(|(_, rule)| {
                format!(
                    "{} {} {}",
                    effect_name(rule.effect),
                    rule.action,
                    rule.pattern
                )
            })
            .collect()
    }

    #[test]
    fn precise_entries_map_exactly_and_the_rest_ask_instead() {
        let (rules, notes) = rules(
            r#"{"permissions": {
                "allow": ["Read(./src/**)", "Bash(cargo:*)", "Bash(npm run test)", "mcp__github__search", "WebFetch(domain:docs.rs)"],
                "ask": ["Edit"],
                "deny": ["Bash(rm:*)", "Bash(git push:*)", "Read(~/.ssh/**)", "Task"]
            }}"#,
            Scope::User,
        );
        assert_eq!(
            shown(&rules),
            [
                "deny process.exec process:rm",
                "ask process.exec process:git",
                "ask fs.write file:**",
                "allow fs.read file:src/**",
                "allow process.exec process:cargo",
                "ask process.exec process:npm",
                "allow mcp.invoke mcp:github/search",
                "allow network.connect host:docs.rs",
            ]
        );
        assert!(rules.iter().all(|(id, _)| id.starts_with("claude/user/")));
        assert!(notes.iter().any(|note| note.contains("`Bash(git push:*)`")));
        assert!(notes.iter().any(|note| note.contains("`Read(~/.ssh/**)`")));
        assert!(notes.iter().any(|note| note.contains("`Task`")));
    }

    #[test]
    fn a_repositorys_rules_carry_the_repositorys_authority() {
        let (rules, _) = rules(r#"{"permissions": {"allow": ["Bash"]}}"#, Scope::Workspace);
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].1.source,
            arsy_kernel::capability::PolicySource::Workspace
        );
    }
}
