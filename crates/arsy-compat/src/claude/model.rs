//! Claude Code's `model` setting.
//!
//! A full model id is carried over for an Anthropic endpoint. An alias —
//! `sonnet`, `opus`, `haiku`, `opusplan` — names whatever Claude currently
//! means by it, which ARSY cannot know, so it is noted rather than guessed.

use crate::{read, Context};
use arsy_kernel::config::{CompatSeed, Dialect, ModelHint};
use std::path::Path;

const ALIASES: &[&str] = &[
    "default",
    "sonnet",
    "opus",
    "haiku",
    "opusplan",
    "sonnet[1m]",
];

pub(crate) fn seeds(context: &Context) -> Vec<CompatSeed> {
    let project = [".claude/settings.local.json", ".claude/settings.json"]
        .map(|relative| Some(context.root.join(relative)));
    let user = context
        .homes
        .claude_dir
        .as_ref()
        .map(|directory| directory.join("settings.json"));
    project
        .into_iter()
        .chain(std::iter::once(user))
        .flatten()
        .filter_map(|path| seed(&path))
        .collect()
}

fn seed(path: &Path) -> Option<CompatSeed> {
    let model = read::json(path)
        .ok()??
        .get("model")?
        .as_str()?
        .trim()
        .to_owned();
    let mut seed = CompatSeed {
        label: "claude".to_owned(),
        path: path.to_path_buf(),
        ..CompatSeed::default()
    };
    if ALIASES.contains(&model.as_str()) {
        seed.notes.push(format!(
            "`model = \"{model}\"` is a Claude alias; set a full model id in arsy.json to use it"
        ));
    } else if !model.is_empty() {
        seed.models.push(ModelHint {
            model,
            dialects: vec![Dialect::Anthropic],
            provider: None,
        });
    }
    Some(seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_id_is_a_hint_and_an_alias_is_a_note() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        std::fs::write(&path, r#"{"model": "claude-opus-4-1"}"#).unwrap();
        let hinted = seed(&path).unwrap();
        assert_eq!(hinted.models[0].model, "claude-opus-4-1");
        assert_eq!(hinted.models[0].dialects, [Dialect::Anthropic]);

        std::fs::write(&path, r#"{"model": "opus"}"#).unwrap();
        let aliased = seed(&path).unwrap();
        assert!(aliased.models.is_empty());
        assert_eq!(aliased.notes.len(), 1);

        std::fs::write(&path, r#"{"permissions": {}}"#).unwrap();
        assert!(seed(&path).is_none());
    }
}
