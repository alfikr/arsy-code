//! Where Claude Code and Codex keep the operator's own files.
//!
//! Resolved from each tool's own variables, so a setup that moved its home
//! with `CLAUDE_CONFIG_DIR` or `CODEX_HOME` is found where that tool finds it.

use std::path::PathBuf;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompatHomes {
    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub claude_dir: Option<PathBuf>,
    /// `~/.claude.json`, which moves into `$CLAUDE_CONFIG_DIR` along with the
    /// directory when that is set.
    pub claude_json: Option<PathBuf>,
    /// `$CODEX_HOME`, else `~/.codex`.
    pub codex_dir: Option<PathBuf>,
}

impl CompatHomes {
    pub fn from_env() -> Self {
        Self::resolve(|name| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
    }

    /// No home at all, for a test that must not read the machine it runs on.
    pub fn none() -> Self {
        Self::default()
    }

    fn resolve(variable: impl Fn(&str) -> Option<PathBuf>) -> Self {
        let home = variable("HOME").or_else(|| variable("USERPROFILE"));
        let claude = variable("CLAUDE_CONFIG_DIR");
        Self {
            claude_json: claude
                .as_ref()
                .or(home.as_ref())
                .map(|directory| directory.join(".claude.json")),
            claude_dir: claude.or_else(|| home.as_ref().map(|home| home.join(".claude"))),
            codex_dir: variable("CODEX_HOME").or_else(|| home.map(|home| home.join(".codex"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(variables: &[(&str, &str)]) -> CompatHomes {
        CompatHomes::resolve(|name| {
            variables
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| PathBuf::from(value))
        })
    }

    #[test]
    fn each_tool_is_found_where_it_looks_for_itself() {
        let plain = resolved(&[("HOME", "/home/op")]);
        assert_eq!(plain.claude_dir, Some(PathBuf::from("/home/op/.claude")));
        assert_eq!(
            plain.claude_json,
            Some(PathBuf::from("/home/op/.claude.json"))
        );
        assert_eq!(plain.codex_dir, Some(PathBuf::from("/home/op/.codex")));

        let moved = resolved(&[
            ("HOME", "/home/op"),
            ("CLAUDE_CONFIG_DIR", "/cfg/claude"),
            ("CODEX_HOME", "/cfg/codex"),
        ]);
        assert_eq!(moved.claude_dir, Some(PathBuf::from("/cfg/claude")));
        assert_eq!(
            moved.claude_json,
            Some(PathBuf::from("/cfg/claude/.claude.json"))
        );
        assert_eq!(moved.codex_dir, Some(PathBuf::from("/cfg/codex")));

        assert_eq!(resolved(&[]), CompatHomes::none());
    }
}
