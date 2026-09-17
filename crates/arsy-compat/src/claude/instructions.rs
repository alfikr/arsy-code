//! Claude Code's user-level `CLAUDE.md`.

use crate::CompatHomes;
use std::path::PathBuf;

pub(crate) fn user_file(homes: &CompatHomes) -> Option<PathBuf> {
    homes
        .claude_dir
        .as_ref()
        .map(|directory| directory.join("CLAUDE.md"))
        .filter(|path| path.is_file())
}
