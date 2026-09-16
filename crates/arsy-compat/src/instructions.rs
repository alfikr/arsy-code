//! The operator's own instruction files, which neither tool keeps in a
//! repository: `$CODEX_HOME/AGENTS.md` and `$CLAUDE_CONFIG_DIR/CLAUDE.md`.

use crate::{read, CompatHomes};
use std::path::PathBuf;

/// One user-level instruction file that was found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserInstruction {
    pub path: PathBuf,
    pub text: String,
    pub truncated: bool,
}

/// Codex's first, then Claude's, each only where its tool is switched on.
/// They come before any repository's instructions, so a project's own
/// guidance is read after — and so overrides — the operator's general one.
pub fn user_instructions(
    homes: &CompatHomes,
    claude: bool,
    codex: bool,
    max_bytes: u64,
) -> Vec<UserInstruction> {
    [
        crate::codex::instructions::user_file(homes).filter(|_| codex),
        crate::claude::instructions::user_file(homes).filter(|_| claude),
    ]
    .into_iter()
    .flatten()
    .filter_map(|path| {
        let (text, truncated) = read::text_prefix(&path, max_bytes)?;
        Some(UserInstruction {
            path,
            text,
            truncated,
        })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_operators_files_are_read_codex_first_and_only_when_switched_on() {
        let home = tempfile::tempdir().unwrap();
        let write = |relative: &str, body: &str| {
            let path = home.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        write(".claude/CLAUDE.md", "claude rule");
        write(".codex/AGENTS.md", "codex rule");
        write(".codex/AGENTS.override.md", "codex override é");
        let homes = CompatHomes {
            claude_dir: Some(home.path().join(".claude")),
            claude_json: None,
            codex_dir: Some(home.path().join(".codex")),
        };

        let texts = |claude, codex, max| -> Vec<(String, bool)> {
            user_instructions(&homes, claude, codex, max)
                .into_iter()
                .map(|found| (found.text, found.truncated))
                .collect()
        };
        assert_eq!(
            texts(true, true, 1024),
            [
                ("codex override é".to_owned(), false),
                ("claude rule".to_owned(), false)
            ]
        );
        assert_eq!(
            texts(true, false, 1024),
            [("claude rule".to_owned(), false)]
        );
        // Cut inside the two-byte `é`, the text stops before it.
        assert_eq!(
            texts(false, true, 16),
            [("codex override ".to_owned(), true)]
        );
        assert!(user_instructions(&CompatHomes::none(), true, true, 1024).is_empty());
    }
}
