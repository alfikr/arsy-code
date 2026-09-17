//! Codex's `sandbox_mode` and `approval_policy`.
//!
//! `read-only` means nothing is written without asking, so writes and deletes
//! ask — or are refused outright under `approval_policy = "never"`, where Codex
//! would never ask either. `workspace-write` is what ARSY's file tools already
//! confine themselves to, and `danger-full-access` grants nothing here: a
//! compatibility file never widens authority.

use crate::{permissions::rule, read, Context, Scope};
use arsy_kernel::{capability::CapabilityAction, config::CompatSeed, policy::RuleEffect};
use std::path::Path;

pub(crate) fn seeds(context: &Context) -> Vec<CompatSeed> {
    let user = context
        .homes
        .codex_dir
        .as_ref()
        .map(|directory| (directory.join("config.toml"), Scope::User));
    let project = Some((context.root.join(".codex/config.toml"), Scope::Workspace));
    [user, project]
        .into_iter()
        .flatten()
        .map(|(path, scope)| config_seed(&path, scope))
        .collect()
}

fn config_seed(path: &Path, scope: Scope) -> CompatSeed {
    let mut seed = CompatSeed {
        label: "codex".to_owned(),
        path: path.to_path_buf(),
        ..CompatSeed::default()
    };
    let config = match read::toml(path) {
        Ok(Some(config)) => config,
        Ok(None) => return seed,
        Err(error) => {
            seed.notes.push(format!("{} {error}", path.display()));
            return seed;
        }
    };
    let text = |key: &str| config.get(key).and_then(toml::Value::as_str);
    let approval = text("approval_policy");
    if config.contains_key("profiles") {
        seed.notes
            .push("`profiles` are not read; only the top-level settings apply".to_owned());
    }
    if approval == Some("never") {
        seed.notes.push(
            "`approval_policy = \"never\"` is not applied: ARSY still asks where its own policy says to"
                .to_owned(),
        );
    }
    match text("sandbox_mode") {
        Some("read-only") => {
            let effect = if approval == Some("never") {
                RuleEffect::Deny
            } else {
                RuleEffect::RequireApproval
            };
            seed.policy_rules.extend(
                [CapabilityAction::FsWrite, CapabilityAction::FsDelete]
                    .into_iter()
                    .filter_map(|action| {
                        let id = format!("codex/{}/sandbox/read-only/{action}", scope.as_str());
                        rule(scope, effect, action, "**").map(|rule| (id, rule))
                    }),
            );
        }
        Some("danger-full-access") => seed
            .notes
            .push("`sandbox_mode = \"danger-full-access\"` grants nothing in ARSY".to_owned()),
        _ => {}
    }
    seed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::effect_name;

    fn shown(config: &str) -> (Vec<String>, Vec<String>) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, config).unwrap();
        let seed = config_seed(&path, Scope::User);
        (
            seed.policy_rules
                .iter()
                .map(|(_, rule)| {
                    format!(
                        "{} {} {}",
                        effect_name(rule.effect),
                        rule.action,
                        rule.pattern
                    )
                })
                .collect(),
            seed.notes,
        )
    }

    #[test]
    fn a_read_only_sandbox_asks_before_writing_and_never_widens() {
        let (rules, _) = shown("sandbox_mode = \"read-only\"\n");
        assert_eq!(rules, ["ask fs.write file:**", "ask fs.delete file:**"]);

        let (rules, notes) = shown("sandbox_mode = \"read-only\"\napproval_policy = \"never\"\n");
        assert_eq!(rules, ["deny fs.write file:**", "deny fs.delete file:**"]);
        assert!(notes.iter().any(|note| note.contains("approval_policy")));

        let (rules, notes) =
            shown("sandbox_mode = \"danger-full-access\"\n[profiles.fast]\nmodel = \"o4\"\n");
        assert!(rules.is_empty());
        assert_eq!(notes.len(), 2);

        assert!(shown("sandbox_mode = \"workspace-write\"\n").0.is_empty());
    }
}
