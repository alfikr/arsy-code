//! Codex's user-level `AGENTS.md`, which `AGENTS.override.md` replaces.

use crate::CompatHomes;
use std::path::PathBuf;

pub(crate) fn user_file(homes: &CompatHomes) -> Option<PathBuf> {
    let directory = homes.codex_dir.as_ref()?;
    ["AGENTS.override.md", "AGENTS.md"]
        .into_iter()
        .map(|name| directory.join(name))
        .find(|path| path.is_file())
}
