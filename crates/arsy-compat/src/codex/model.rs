//! Codex's top-level `model` and `model_provider`.
//!
//! Codex speaks OpenAI's dialects. A `model_provider` other than Codex's own
//! default names a provider table, so the model is kept for the ARSY endpoint
//! of that id only.

use crate::{read, Context};
use arsy_kernel::config::{CompatSeed, Dialect, ModelHint};
use std::path::Path;

pub(crate) fn seeds(context: &Context) -> Vec<CompatSeed> {
    let user = context
        .homes
        .codex_dir
        .as_ref()
        .map(|directory| directory.join("config.toml"));
    [Some(context.root.join(".codex/config.toml")), user]
        .into_iter()
        .flatten()
        .filter_map(|path| seed(&path))
        .collect()
}

fn seed(path: &Path) -> Option<CompatSeed> {
    let config = read::toml(path).ok()??;
    let model = config.get("model")?.as_str()?.trim().to_owned();
    let provider = config
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .filter(|provider| *provider != "openai")
        .map(str::to_owned);
    (!model.is_empty()).then(|| CompatSeed {
        label: "codex".to_owned(),
        path: path.to_path_buf(),
        models: vec![ModelHint {
            model,
            dialects: vec![Dialect::Openai, Dialect::OpenaiResponses],
            provider,
        }],
        ..CompatSeed::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_provider_narrows_the_hint_to_that_endpoint() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "model = \"gpt-5-codex\"\n").unwrap();
        let plain = seed(&path).unwrap();
        assert_eq!(plain.models[0].model, "gpt-5-codex");
        assert_eq!(plain.models[0].provider, None);

        std::fs::write(
            &path,
            "model = \"qwen3-coder\"\nmodel_provider = \"gateway\"\n",
        )
        .unwrap();
        assert_eq!(
            seed(&path).unwrap().models[0].provider.as_deref(),
            Some("gateway")
        );
    }
}
