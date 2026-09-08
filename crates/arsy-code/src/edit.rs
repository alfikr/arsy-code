use crate::intelligence::SymbolId;
use arsy_kernel::domain::{StateVersion, WorkspaceVersion};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fmt, fs, io,
    num::NonZeroU32,
    ops::Range,
    path::{Component, Path, PathBuf},
};

const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OPERATIONS: usize = 256;
const MAX_TRANSACTION_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum EditAddress {
    /// Replaces the whole file after checking its current and replacement digests.
    ContentAnchor {
        before: StateVersion,
        after: StateVersion,
    },
    /// Replaces a unique match, or a caller-selected one-based occurrence.
    TextAnchor {
        needle: String,
        occurrence: Option<NonZeroU32>,
    },
    /// Re-resolves the symbol and accepts a moved range only when semantic
    /// identity still matches the address selected by the caller.
    Symbol {
        index: StateVersion,
        symbol: SymbolId,
        identity: StateVersion,
    },
    /// One file's portion of an LSP WorkspaceEdit. Transactions combine the
    /// per-file portions atomically.
    WorkspaceEdit {
        server: String,
        revision: StateVersion,
        edits: Vec<RangeReplacement>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RangeReplacement {
    pub bytes: Range<usize>,
    pub replacement: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSymbol {
    pub index: StateVersion,
    pub identity: StateVersion,
    pub bytes: Range<usize>,
}

pub trait SemanticResolver {
    fn resolve_symbol(&self, path: &Path, symbol: &SymbolId) -> Result<ResolvedSymbol, EditError>;

    fn server_revision(&self, server: &str) -> Result<StateVersion, EditError>;
}

struct NoSemanticResolver;

impl SemanticResolver for NoSemanticResolver {
    fn resolve_symbol(
        &self,
        _path: &Path,
        _symbol: &SymbolId,
    ) -> Result<ResolvedSymbol, EditError> {
        Err(EditError::SemanticUnavailable)
    }

    fn server_revision(&self, _server: &str) -> Result<StateVersion, EditError> {
        Err(EditError::SemanticUnavailable)
    }
}

#[derive(Clone, Debug)]
pub struct EditOperation {
    pub path: PathBuf,
    pub address: EditAddress,
    pub replacement: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct EditTransaction {
    pub base: WorkspaceVersion,
    pub operations: Vec<EditOperation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEdit {
    pub path: PathBuf,
    pub before: StateVersion,
    pub after: StateVersion,
    /// Ranges changed by an optional formatter outside the approved edit.
    /// The core engine does not invoke a formatter, so this is empty here.
    pub undeclared_changes: Vec<Range<usize>>,
}

#[derive(Debug)]
pub enum EditError {
    StaleBase {
        expected: WorkspaceVersion,
        actual: WorkspaceVersion,
    },
    InvalidPath(PathBuf),
    DuplicatePath(PathBuf),
    TransactionTooLarge,
    FileTooLarge(PathBuf),
    StaleContent(PathBuf),
    ReplacementDigest(PathBuf),
    AnchorNotFound(PathBuf),
    SemanticUnavailable,
    SemanticIdentityChanged(PathBuf),
    StaleServerRevision(String),
    InvalidRange(PathBuf),
    OverlappingRanges(PathBuf),
    Ambiguous {
        path: PathBuf,
        candidates: Vec<usize>,
    },
    Io(io::Error),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleBase { .. }
            | Self::InvalidPath(_)
            | Self::DuplicatePath(_)
            | Self::TransactionTooLarge
            | Self::FileTooLarge(_)
            | Self::StaleContent(_)
            | Self::ReplacementDigest(_)
            | Self::AnchorNotFound(_) => display_basic_error(self, f),
            Self::SemanticUnavailable
            | Self::SemanticIdentityChanged(_)
            | Self::StaleServerRevision(_)
            | Self::InvalidRange(_)
            | Self::OverlappingRanges(_) => display_semantic_error(self, f),
            Self::Ambiguous { path, candidates } => write!(
                f,
                "text anchor in {} has candidates at {candidates:?}",
                path.display()
            ),
            Self::Io(error) => error.fmt(f),
        }
    }
}

fn display_basic_error(error: &EditError, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match error {
        EditError::StaleBase { .. } => f.write_str("workspace base version does not match"),
        EditError::InvalidPath(path) => write!(f, "invalid workspace path: {}", path.display()),
        EditError::DuplicatePath(path) => write!(f, "multiple edits target {}", path.display()),
        EditError::TransactionTooLarge => {
            f.write_str("edit transaction exceeds its bounded limits")
        }
        EditError::FileTooLarge(path) => write!(f, "file exceeds edit limit: {}", path.display()),
        EditError::StaleContent(path) => write!(f, "content anchor is stale: {}", path.display()),
        EditError::ReplacementDigest(path) => write!(
            f,
            "replacement digest differs from anchor: {}",
            path.display()
        ),
        EditError::AnchorNotFound(path) => {
            write!(f, "text anchor not found: {}", path.display())
        }
        _ => unreachable!("caller passes only basic edit errors"),
    }
}

fn display_semantic_error(error: &EditError, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match error {
        EditError::SemanticUnavailable => {
            f.write_str("semantic edit needs a code-intelligence resolver")
        }
        EditError::SemanticIdentityChanged(path) => {
            write!(f, "symbol identity changed in {}", path.display())
        }
        EditError::StaleServerRevision(server) => {
            write!(f, "workspace edit from {server} is stale")
        }
        EditError::InvalidRange(path) => {
            write!(f, "edit range is invalid in {}", path.display())
        }
        EditError::OverlappingRanges(path) => {
            write!(f, "workspace edit ranges overlap in {}", path.display())
        }
        _ => unreachable!("caller passes only semantic edit errors"),
    }
}

impl std::error::Error for EditError {}

impl From<io::Error> for EditError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn workspace_version(root: &Path) -> Result<WorkspaceVersion, EditError> {
    let root = fs::canonicalize(root)?;
    let mut files = Vec::new();
    collect_files(&root, &root, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for path in files {
        let relative = path.strip_prefix(&root).expect("collected below root");
        let bytes = read_bounded(&path, relative)?;
        hasher.update(relative.as_os_str().as_encoded_bytes());
        hasher.update([0]);
        hasher.update(Sha256::digest(bytes));
    }
    Ok(WorkspaceVersion(StateVersion::from_digest(
        hasher.finalize().into(),
    )))
}

pub fn apply(root: &Path, transaction: &EditTransaction) -> Result<Vec<FileEdit>, EditError> {
    apply_with_resolver(root, transaction, &NoSemanticResolver)
}

pub fn apply_with_resolver(
    root: &Path,
    transaction: &EditTransaction,
    resolver: &dyn SemanticResolver,
) -> Result<Vec<FileEdit>, EditError> {
    let root = fs::canonicalize(root)?;
    let actual = workspace_version(&root)?;
    if actual != transaction.base {
        return Err(EditError::StaleBase {
            expected: transaction.base,
            actual,
        });
    }
    stage_and_commit(&root, &transaction.operations, resolver)
}

/// Apply operations without first proving the whole workspace is unchanged.
///
/// [`workspace_version`] hashes every tracked file, which is the right
/// precondition for a queued transaction replayed later and the wrong one for
/// an agent that edits a file it read a moment ago: it costs a full tree read
/// per edit, and it fails when an unrelated file changed. Each address here
/// already carries its own precondition — a content digest, or an anchor that
/// must still be present exactly once — so staleness is caught per file, where
/// it actually matters.
pub fn apply_unversioned(
    root: &Path,
    operations: &[EditOperation],
) -> Result<Vec<FileEdit>, EditError> {
    let root = fs::canonicalize(root)?;
    stage_and_commit(&root, operations, &NoSemanticResolver)
}

fn stage_and_commit(
    root: &Path,
    operations: &[EditOperation],
    resolver: &dyn SemanticResolver,
) -> Result<Vec<FileEdit>, EditError> {
    if operations.len() > MAX_OPERATIONS {
        return Err(EditError::TransactionTooLarge);
    }
    let mut seen = HashSet::new();
    let mut prepared = Vec::with_capacity(operations.len());
    let mut staged_bytes = 0usize;
    for operation in operations {
        let requested = confined(&operation.path)?;
        let path = fs::canonicalize(root.join(&requested))?;
        let relative = path
            .strip_prefix(root)
            .map_err(|_| EditError::InvalidPath(requested))?
            .to_owned();
        if !seen.insert(relative.clone()) {
            return Err(EditError::DuplicatePath(relative));
        }
        let before_bytes = read_bounded(&path, &relative)?;
        let before = digest(&before_bytes);
        let output = resolve(operation, &relative, &before_bytes, before, resolver)?;
        staged_bytes = staged_bytes
            .checked_add(output.len())
            .filter(|size| *size <= MAX_TRANSACTION_BYTES)
            .ok_or(EditError::TransactionTooLarge)?;
        let after = digest(&output);
        prepared.push((relative, path, output, before, after));
    }

    let stage = tempfile::Builder::new()
        .prefix(".arsy-edit-")
        .tempdir_in(root)?;
    for (relative, path, output, _, _) in &prepared {
        let staged = stage.path().join(relative);
        if let Some(parent) = staged.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&staged, output)?;
        fs::set_permissions(&staged, fs::metadata(path)?.permissions())?;
    }

    let backup = stage.path().join("backup");
    fs::create_dir(&backup)?;
    let mut committed = Vec::new();
    for (relative, path, _, _, _) in &prepared {
        let saved = backup.join(relative);
        if let Some(parent) = saved.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Err(error) =
            fs::rename(path, &saved).and_then(|()| fs::rename(stage.path().join(relative), path))
        {
            if saved.exists() {
                let _ = fs::rename(&saved, path);
            }
            for (done_path, done_saved) in committed.into_iter().rev() {
                let _ = fs::remove_file(&done_path);
                let _ = fs::rename(done_saved, done_path);
            }
            return Err(EditError::Io(error));
        }
        committed.push((path.clone(), saved));
    }

    Ok(prepared
        .into_iter()
        .map(|(path, _, _, before, after)| FileEdit {
            path,
            before,
            after,
            undeclared_changes: Vec::new(),
        })
        .collect())
}

fn resolve(
    operation: &EditOperation,
    path: &Path,
    input: &[u8],
    before: StateVersion,
    resolver: &dyn SemanticResolver,
) -> Result<Vec<u8>, EditError> {
    match &operation.address {
        EditAddress::ContentAnchor {
            before: expected,
            after,
        } => {
            if &before != expected {
                return Err(EditError::StaleContent(path.to_owned()));
            }
            if digest(&operation.replacement) != *after {
                return Err(EditError::ReplacementDigest(path.to_owned()));
            }
            Ok(operation.replacement.clone())
        }
        EditAddress::TextAnchor { needle, occurrence } => {
            let needle = needle.as_bytes();
            if needle.is_empty() {
                return Err(EditError::AnchorNotFound(path.to_owned()));
            }
            let candidates: Vec<_> = input
                .windows(needle.len())
                .enumerate()
                .filter_map(|(i, value)| (value == needle).then_some(i))
                .collect();
            let offset = match occurrence {
                Some(value) => candidates.get(value.get() as usize - 1).copied(),
                None if candidates.len() == 1 => candidates.first().copied(),
                None if candidates.len() > 1 => {
                    return Err(EditError::Ambiguous {
                        path: path.to_owned(),
                        candidates,
                    })
                }
                None => None,
            }
            .ok_or_else(|| EditError::AnchorNotFound(path.to_owned()))?;
            let mut output =
                Vec::with_capacity(input.len() - needle.len() + operation.replacement.len());
            output.extend_from_slice(&input[..offset]);
            output.extend_from_slice(&operation.replacement);
            output.extend_from_slice(&input[offset + needle.len()..]);
            Ok(output)
        }
        EditAddress::Symbol {
            index,
            symbol,
            identity,
        } => {
            let resolved = resolver.resolve_symbol(path, symbol)?;
            if &resolved.identity != identity {
                return Err(EditError::SemanticIdentityChanged(path.to_owned()));
            }
            // A changed index is expected after unrelated edits. Re-resolution
            // above supplies the current range; identity is the safety check.
            let _stale_index = resolved.index != *index;
            replace_ranges(
                input,
                path,
                &[RangeReplacement {
                    bytes: resolved.bytes,
                    replacement: operation.replacement.clone(),
                }],
            )
        }
        EditAddress::WorkspaceEdit {
            server,
            revision,
            edits,
        } => {
            if resolver.server_revision(server)? != *revision {
                return Err(EditError::StaleServerRevision(server.clone()));
            }
            replace_ranges(input, path, edits)
        }
    }
}

fn replace_ranges(
    input: &[u8],
    path: &Path,
    edits: &[RangeReplacement],
) -> Result<Vec<u8>, EditError> {
    if edits.is_empty() {
        return Err(EditError::InvalidRange(path.to_owned()));
    }
    let mut edits = edits.iter().collect::<Vec<_>>();
    edits.sort_by_key(|edit| edit.bytes.start);
    if edits
        .iter()
        .any(|edit| edit.bytes.start > edit.bytes.end || edit.bytes.end > input.len())
    {
        return Err(EditError::InvalidRange(path.to_owned()));
    }
    if edits
        .windows(2)
        .any(|pair| pair[0].bytes.end > pair[1].bytes.start)
    {
        return Err(EditError::OverlappingRanges(path.to_owned()));
    }
    let mut output = input.to_vec();
    for edit in edits.into_iter().rev() {
        output.splice(edit.bytes.clone(), edit.replacement.iter().copied());
    }
    Ok(output)
}

fn confined(path: &Path) -> Result<PathBuf, EditError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(EditError::InvalidPath(path.to_owned()));
    }
    Ok(path.to_owned())
}

fn read_bounded(path: &Path, display: &Path) -> Result<Vec<u8>, EditError> {
    if fs::metadata(path)?.len() > MAX_FILE_BYTES {
        return Err(EditError::FileTooLarge(display.to_owned()));
    }
    Ok(fs::read(path)?)
}

fn digest(bytes: &[u8]) -> StateVersion {
    StateVersion::from_digest(Sha256::digest(bytes).into())
}

fn collect_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), EditError> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect_files(root, &entry.path(), files)?;
        } else if kind.is_file() && entry.path().strip_prefix(root).is_ok() {
            files.push(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Resolver {
        symbol: ResolvedSymbol,
        server: StateVersion,
    }

    impl SemanticResolver for Resolver {
        fn resolve_symbol(
            &self,
            _path: &Path,
            _symbol: &SymbolId,
        ) -> Result<ResolvedSymbol, EditError> {
            Ok(self.symbol.clone())
        }

        fn server_revision(&self, _server: &str) -> Result<StateVersion, EditError> {
            Ok(self.server)
        }
    }

    #[test]
    fn preflights_all_files_and_applies_a_multi_file_edit() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a"), "one old").unwrap();
        fs::write(temp.path().join("b"), "two old").unwrap();
        let base = workspace_version(temp.path()).unwrap();
        let transaction = EditTransaction {
            base,
            operations: vec![
                EditOperation {
                    path: "a".into(),
                    address: EditAddress::TextAnchor {
                        needle: "old".into(),
                        occurrence: None,
                    },
                    replacement: b"new".to_vec(),
                },
                EditOperation {
                    path: "b".into(),
                    address: EditAddress::TextAnchor {
                        needle: "old".into(),
                        occurrence: None,
                    },
                    replacement: b"new".to_vec(),
                },
            ],
        };

        let edits = apply(temp.path(), &transaction).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("a")).unwrap(),
            "one new"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("b")).unwrap(),
            "two new"
        );
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(|edit| edit.before != edit.after));
        assert!(edits.iter().all(|edit| edit.undeclared_changes.is_empty()));
    }

    #[test]
    fn stale_or_ambiguous_preflight_leaves_every_file_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a"), "old old").unwrap();
        fs::write(temp.path().join("b"), "untouched").unwrap();
        let base = workspace_version(temp.path()).unwrap();
        let transaction = EditTransaction {
            base,
            operations: vec![
                EditOperation {
                    path: "b".into(),
                    address: EditAddress::TextAnchor {
                        needle: "untouched".into(),
                        occurrence: None,
                    },
                    replacement: b"changed".to_vec(),
                },
                EditOperation {
                    path: "a".into(),
                    address: EditAddress::TextAnchor {
                        needle: "old".into(),
                        occurrence: None,
                    },
                    replacement: b"new".to_vec(),
                },
            ],
        };

        let error = apply(temp.path(), &transaction).unwrap_err();
        assert!(
            matches!(error, EditError::Ambiguous { candidates, .. } if candidates == vec![0, 4])
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("b")).unwrap(),
            "untouched"
        );

        fs::write(temp.path().join("b"), "external").unwrap();
        assert!(matches!(
            apply(temp.path(), &transaction),
            Err(EditError::StaleBase { .. })
        ));
    }

    #[test]
    fn content_anchor_checks_before_and_after_digests() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a"), "before").unwrap();
        let replacement = b"after".to_vec();
        let transaction = EditTransaction {
            base: workspace_version(temp.path()).unwrap(),
            operations: vec![EditOperation {
                path: "a".into(),
                address: EditAddress::ContentAnchor {
                    before: digest(b"before"),
                    after: digest(&replacement),
                },
                replacement,
            }],
        };

        apply(temp.path(), &transaction).unwrap();
        assert_eq!(fs::read_to_string(temp.path().join("a")).unwrap(), "after");
    }

    #[test]
    fn stale_symbol_is_reresolved_only_when_identity_matches() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a"), "fn old() {}\n").unwrap();
        let identity = StateVersion::from_digest([3; 32]);
        let resolver = Resolver {
            symbol: ResolvedSymbol {
                index: StateVersion::from_digest([2; 32]),
                identity,
                bytes: 3..6,
            },
            server: StateVersion::from_digest([4; 32]),
        };
        let transaction = EditTransaction {
            base: workspace_version(temp.path()).unwrap(),
            operations: vec![EditOperation {
                path: "a".into(),
                address: EditAddress::Symbol {
                    index: StateVersion::from_digest([1; 32]),
                    symbol: SymbolId::new("old").unwrap(),
                    identity,
                },
                replacement: b"new".to_vec(),
            }],
        };

        apply_with_resolver(temp.path(), &transaction, &resolver).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("a")).unwrap(),
            "fn new() {}\n"
        );

        let mut wrong = resolver;
        wrong.symbol.identity = StateVersion::from_digest([9; 32]);
        let transaction = EditTransaction {
            base: workspace_version(temp.path()).unwrap(),
            operations: vec![EditOperation {
                path: "a".into(),
                address: EditAddress::Symbol {
                    index: StateVersion::from_digest([2; 32]),
                    symbol: SymbolId::new("new").unwrap(),
                    identity,
                },
                replacement: b"bad".to_vec(),
            }],
        };
        assert!(matches!(
            apply_with_resolver(temp.path(), &transaction, &wrong),
            Err(EditError::SemanticIdentityChanged(_))
        ));
        assert_eq!(
            fs::read_to_string(temp.path().join("a")).unwrap(),
            "fn new() {}\n"
        );
    }

    #[test]
    fn workspace_edits_validate_revision_and_apply_atomically() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a"), "old + old\n").unwrap();
        fs::write(temp.path().join("b"), "old\n").unwrap();
        let revision = StateVersion::from_digest([4; 32]);
        let resolver = Resolver {
            symbol: ResolvedSymbol {
                index: revision,
                identity: revision,
                bytes: 0..0,
            },
            server: revision,
        };
        let transaction = EditTransaction {
            base: workspace_version(temp.path()).unwrap(),
            operations: vec![
                EditOperation {
                    path: "a".into(),
                    address: EditAddress::WorkspaceEdit {
                        server: "rust-analyzer".into(),
                        revision,
                        edits: vec![
                            RangeReplacement {
                                bytes: 0..3,
                                replacement: b"new".to_vec(),
                            },
                            RangeReplacement {
                                bytes: 6..9,
                                replacement: b"new".to_vec(),
                            },
                        ],
                    },
                    replacement: Vec::new(),
                },
                EditOperation {
                    path: "b".into(),
                    address: EditAddress::WorkspaceEdit {
                        server: "rust-analyzer".into(),
                        revision,
                        edits: vec![RangeReplacement {
                            bytes: 0..3,
                            replacement: b"new".to_vec(),
                        }],
                    },
                    replacement: Vec::new(),
                },
            ],
        };

        apply_with_resolver(temp.path(), &transaction, &resolver).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("a")).unwrap(),
            "new + new\n"
        );
        assert_eq!(fs::read_to_string(temp.path().join("b")).unwrap(), "new\n");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_an_edit_through_a_symlink_outside_the_workspace() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "outside").unwrap();
        symlink(outside.path(), temp.path().join("link")).unwrap();
        let transaction = EditTransaction {
            base: workspace_version(temp.path()).unwrap(),
            operations: vec![EditOperation {
                path: "link".into(),
                address: EditAddress::TextAnchor {
                    needle: "outside".into(),
                    occurrence: None,
                },
                replacement: b"changed".to_vec(),
            }],
        };

        assert!(matches!(
            apply(temp.path(), &transaction),
            Err(EditError::InvalidPath(_))
        ));
        assert_eq!(fs::read_to_string(outside.path()).unwrap(), "outside");
    }
}
