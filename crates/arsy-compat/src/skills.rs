//! The operator's user-level skills, from [CC]'s and Codex's own homes.
//!
//! A user skill lives at `<claude home>/skills/<name>/SKILL.md` and
//! `<codex home>/skills/<name>/SKILL.md`. Workspace skills are a different
//! discovery — the compatibility importer reads those, inside the workspace —
//! and the two are kept apart so a workspace cannot widen what the operator's
//! own home offers.

use crate::CompatHomes;
use std::path::PathBuf;

/// One user-scope skill: its name and the `SKILL.md` that declares it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserSkill {
    /// The directory name, which is also what the model addresses it by.
    pub name: String,
    /// Where the skill came from, as it is written in a listing.
    pub ecosystem: &'static str,
    /// The `SKILL.md` to read.
    pub path: PathBuf,
}

/// Every user-level skill directory that holds a `SKILL.md` per child.
fn scan(directory: PathBuf, ecosystem: &'static str) -> Vec<UserSkill> {
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut skills = Vec::new();
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        // A symlinked skill directory counts: an operator keeping skills in
        // one tree and linking them into both homes is ordinary.
        if !kind.is_dir() && !kind.is_symlink() {
            continue;
        }
        let path = entry.path().join("SKILL.md");
        if !path.is_file() {
            continue;
        }
        // The name the model addresses is the front matter's when the file
        // declares one, and the directory name otherwise — the same rule the
        // ecosystems themselves use.
        let name = declared_name(&path)
            .unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
        skills.push(UserSkill {
            name,
            ecosystem,
            path,
        });
    }
    skills
}

/// The `name:` line of a `SKILL.md`'s front matter, when it has one.
///
/// Front matter is the first `---` block; anything else is a skill without a
/// declared name, which is allowed.
fn declared_name(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let body = text.strip_prefix("---")?;
    let front = body.split("---").next()?;
    front.lines().find_map(|line| {
        let value = line.strip_prefix("name:")?;
        let name = value.trim().trim_matches('"').trim_matches('\'');
        (!name.is_empty()).then(|| name.to_owned())
    })
}

/// [CC]'s user skills, or none when `claude` is off.
pub fn claude(homes: &CompatHomes, enabled: bool) -> Vec<UserSkill> {
    homes
        .claude_dir
        .as_ref()
        .filter(|_| enabled)
        .map(|directory| scan(directory.join("skills"), "claude"))
        .unwrap_or_default()
}

/// Codex's user skills, or none when `codex` is off.
pub fn codex(homes: &CompatHomes, enabled: bool) -> Vec<UserSkill> {
    homes
        .codex_dir
        .as_ref()
        .filter(|_| enabled)
        .map(|directory| scan(directory.join("skills"), "codex"))
        .unwrap_or_default()
}

/// Every user-level skill the operator's two homes offer, in the order the
/// compat switches run: codex first, then claude.
pub fn all(homes: &CompatHomes, claude_on: bool, codex_on: bool) -> Vec<UserSkill> {
    let mut skills = codex(homes, codex_on);
    skills.extend(claude(homes, claude_on));
    skills
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home layout both ecosystems read the same way: `skills/<dir>/SKILL.md`.
    #[test]
    fn skills_are_found_under_either_home_and_named_from_front_matter() {
        let home = tempfile::tempdir().unwrap();
        let claude_dir = home.path().join(".claude/skills/review");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("SKILL.md"),
            "---\nname: code-review\ndescription: reviews code\n---\nbody",
        )
        .unwrap();
        let codex_dir = home.path().join(".codex/skills/deploy");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(codex_dir.join("SKILL.md"), "no front matter").unwrap();
        // A directory without a SKILL.md is not a skill.
        std::fs::create_dir_all(home.path().join(".claude/skills/empty")).unwrap();

        let homes = CompatHomes {
            claude_dir: Some(home.path().join(".claude")),
            codex_dir: Some(home.path().join(".codex")),
            ..crate::CompatHomes::none()
        };
        let found = all(&homes, true, true);
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["deploy", "code-review"], "codex first, sorted");
        assert_eq!(found[0].ecosystem, "codex");
        assert_eq!(found[1].ecosystem, "claude");
        // The front matter's name wins over the directory it sits in.
        assert!(found[1].path.ends_with("review/SKILL.md"));
    }

    /// A switch that is off means that home contributes nothing: the same
    /// switch governs a source's skills, instructions, and hooks.
    #[test]
    fn a_switched_off_source_contributes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/skills/review");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "body").unwrap();
        let homes = CompatHomes {
            claude_dir: Some(home.path().join(".claude")),
            codex_dir: Some(home.path().join(".codex")),
            ..crate::CompatHomes::none()
        };
        assert!(all(&homes, false, true).is_empty());
        assert!(all(&homes, true, true).len() == 1);
    }
}
