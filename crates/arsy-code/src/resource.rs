//! The one place a workspace path is resolved.
//!
//! Every file a tool touches goes through [`Workspace`], which owns a cap-std
//! [`Dir`] for the root. Confinement is therefore two independent checks: a
//! lexical one that refuses `..`, absolute paths, and prefixes, and the
//! syscall-level one cap-std performs on every operation, which is what stops a
//! symlink whose name looks local from resolving outside the tree. Neither
//! check alone is enough, and nothing else in the crate is allowed to open a
//! workspace path by another route.

use arsy_kernel::domain::{ResourceRef, ResourceRefError, StateVersion};
use cap_std::{ambient_authority, fs::Dir};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::File,
    io,
    path::{Component, Path, PathBuf},
};

/// A workspace file resolved once for both policy and execution.
pub struct ResolvedFile {
    resource: ResourceRef,
    file: File,
}

#[derive(Debug, Eq, PartialEq)]
pub struct FileContent {
    pub bytes: Vec<u8>,
    pub digest: StateVersion,
}

impl FileContent {
    /// Whether the bytes are text a model can be shown.
    ///
    /// A NUL byte is the cheap, near-certain marker of a binary file, and it is
    /// the same test the searcher uses, so a file is never text to one and
    /// binary to the other.
    pub fn is_binary(&self) -> bool {
        self.bytes.contains(&0) || std::str::from_utf8(&self.bytes).is_err()
    }
}

/// The harness's own state, kept out of listings, searches, and file finds.
pub const STATE_DIRECTORY: &str = ".arsy";

/// One entry of a directory listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub directory: bool,
    pub bytes: u64,
}

impl ResolvedFile {
    pub fn resource(&self) -> &ResourceRef {
        &self.resource
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn into_file(self) -> File {
        self.file
    }

    pub fn read(self, max_bytes: u64) -> io::Result<FileContent> {
        use io::Read;

        let mut bytes = Vec::new();
        self.file
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "file exceeds read limit",
            ));
        }
        let digest = arsy_kernel::domain::StateVersion::from_digest(Sha256::digest(&bytes).into());
        Ok(FileContent { bytes, digest })
    }
}

/// A capability directory that confines all path resolution to one workspace.
pub struct Workspace {
    root: Dir,
    path: PathBuf,
}

/// Drop Windows' extended-length prefix from a canonicalized path.
///
/// `canonicalize` returns `\\?\C:\...` on Windows, and nothing else produces
/// that spelling: not a URI from a language server, not an argument from the
/// operator. Every comparison against the workspace root would then fail
/// against a path naming the very same file, and a file inside the workspace
/// would be reported as escaping it.
///
/// A UNC share canonicalizes to `\\?\UNC\server\share`, which needs a
/// different rewrite to be usable, so those are left as they are.
#[cfg_attr(not(windows), allow(dead_code))]
fn without_verbatim_prefix(path: &str) -> Option<&str> {
    path.strip_prefix(r"\\?\")
        .filter(|rest| !rest.starts_with("UNC\\"))
}

impl Workspace {
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let path = std::fs::canonicalize(root)?;
        #[cfg(windows)]
        let path = match path.to_str().and_then(without_verbatim_prefix) {
            Some(plain) => PathBuf::from(plain),
            None => path,
        };
        Dir::open_ambient_dir(&path, ambient_authority()).map(|root| Self { root, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The workspace-relative resource a path names, without opening it.
    ///
    /// Writes need a name for a file that does not exist yet, which
    /// [`Self::resolve_file`] cannot give: it canonicalizes, and canonicalizing
    /// a missing path fails. The lexical check is the same one; the syscall
    /// check happens when cap-std performs the operation.
    fn relative(&self, path: impl AsRef<Path>) -> Result<PathBuf, ResolveError> {
        confined(path.as_ref())
    }

    /// Read a whole file, bounded.
    pub fn read(
        &self,
        path: impl AsRef<Path>,
        max_bytes: u64,
    ) -> Result<FileContent, ResolveError> {
        Ok(self.resolve_file(path)?.read(max_bytes)?)
    }

    /// Replace a file's contents, creating it and any missing parent.
    ///
    /// Returns the digest of what was written, which is the precondition a
    /// later edit of the same file is checked against.
    pub fn write(
        &self,
        path: impl AsRef<Path>,
        bytes: &[u8],
    ) -> Result<StateVersion, ResolveError> {
        let relative = self.relative(path)?;
        if let Some(parent) = relative
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            self.root.create_dir_all(parent)?;
        }
        self.root.write(&relative, bytes)?;
        Ok(StateVersion::from_digest(Sha256::digest(bytes).into()))
    }

    /// Whether a path names something that exists, without opening it.
    ///
    /// A confinement failure is not an existence question, so it answers
    /// `false` rather than propagating: a caller asking "was this here before I
    /// wrote it" gets an answer, and the write itself is what refuses a path
    /// outside the workspace.
    pub fn exists(&self, path: impl AsRef<Path>) -> bool {
        self.relative(path)
            .is_ok_and(|relative| self.root.metadata(relative).is_ok())
    }

    /// Create a file that must not already exist.
    pub fn create_new(
        &self,
        path: impl AsRef<Path>,
        bytes: &[u8],
    ) -> Result<StateVersion, ResolveError> {
        let relative = self.relative(path)?;
        if self.exists(&relative) {
            return Err(ResolveError::AlreadyExists);
        }
        self.write(&relative, bytes)
    }

    pub fn remove(&self, path: impl AsRef<Path>) -> Result<(), ResolveError> {
        let relative = self.relative(path)?;
        Ok(self.root.remove_file(&relative)?)
    }

    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<(), ResolveError> {
        let from = self.relative(from)?;
        let to = self.relative(to)?;
        if self.exists(&to) {
            return Err(ResolveError::AlreadyExists);
        }
        if let Some(parent) = to.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            self.root.create_dir_all(parent)?;
        }
        Ok(self.root.rename(&from, &self.root, &to)?)
    }

    /// One directory level, sorted, directories marked.
    ///
    /// A relative path of `.` lists the root, which is the only way to name it
    /// through a checker that refuses an empty path.
    ///
    /// The harness's own state directory is left out: `.arsy` holds the session
    /// store and the artifacts of the very calls being made, so listing it
    /// shows the model its own exhaust and invites it to read or edit that
    /// instead of the project.
    pub fn list(&self, path: impl AsRef<Path>) -> Result<Vec<DirEntry>, ResolveError> {
        let path = path.as_ref();
        let root_level = path.as_os_str().is_empty() || path == Path::new(".");
        let entries = if root_level {
            self.root.entries()?
        } else {
            self.root.read_dir(self.relative(path)?)?
        };
        let mut listed = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if root_level && name == STATE_DIRECTORY {
                continue;
            }
            let metadata = entry.metadata()?;
            listed.push(DirEntry {
                name,
                directory: metadata.is_dir(),
                bytes: metadata.len(),
            });
        }
        listed.sort_by(|left, right| {
            right
                .directory
                .cmp(&left.directory)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(listed)
    }

    pub fn resolve_file(&self, path: impl AsRef<Path>) -> Result<ResolvedFile, ResolveError> {
        let path = confined(path.as_ref())?;
        let canonical = self.root.canonicalize(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::PermissionDenied {
                ResolveError::OutsideWorkspace
            } else {
                ResolveError::Io(error)
            }
        })?;
        let canonical = confined(&canonical)?;
        let file = self.root.open(&canonical)?.into_std();
        let value = canonical
            .to_str()
            .ok_or(ResolveError::NonUtf8)?
            .replace('\\', "/");

        Ok(ResolvedFile {
            resource: ResourceRef::new("workspace", value)?,
            file,
        })
    }
}

/// The one walk every repository-wide traversal uses.
///
/// Honours `.gitignore` and the other standard filters, and skips the
/// harness's own state directory — so a listing, a text search, and a file find
/// agree about what the workspace contains rather than each drawing its own
/// boundary.
pub fn walk(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != std::ffi::OsStr::new(STATE_DIRECTORY))
        .build()
}

fn confined(path: &Path) -> Result<PathBuf, ResolveError> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ResolveError::OutsideWorkspace)
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(ResolveError::EmptyPath);
    }
    Ok(relative)
}

#[derive(Debug)]
pub enum ResolveError {
    OutsideWorkspace,
    EmptyPath,
    NonUtf8,
    AlreadyExists,
    Io(io::Error),
    Resource(ResourceRefError),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideWorkspace => formatter.write_str("path escapes the workspace"),
            Self::EmptyPath => formatter.write_str("path must name a workspace file"),
            Self::NonUtf8 => formatter.write_str("canonical workspace path is not UTF-8"),
            Self::AlreadyExists => formatter.write_str("a file already exists at that path"),
            Self::Io(error) => error.fmt(formatter),
            Self::Resource(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<io::Error> for ResolveError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<ResourceRefError> for ResolveError {
    fn from(value: ResourceRefError) -> Self {
        Self::Resource(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn resolves_a_workspace_file_to_an_open_handle() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "original").unwrap();
        let workspace = Workspace::open(temp.path()).unwrap();

        let resolved = workspace.resolve_file("src/./lib.rs").unwrap();
        let mut text = String::new();
        (&resolved.file).read_to_string(&mut text).unwrap();

        assert_eq!(resolved.resource.value(), "src/lib.rs");
        assert_eq!(text, "original");
    }

    #[cfg(unix)]
    #[test]
    fn execution_keeps_the_handle_that_policy_inspected() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("file"), "approved").unwrap();
        let resolved = Workspace::open(temp.path())
            .unwrap()
            .resolve_file("file")
            .unwrap();

        std::fs::rename(temp.path().join("file"), temp.path().join("moved")).unwrap();
        std::fs::write(temp.path().join("file"), "replacement").unwrap();
        let mut text = String::new();
        (&resolved.file).read_to_string(&mut text).unwrap();

        assert_eq!(text, "approved");
    }

    #[test]
    fn rejects_parent_and_absolute_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(temp.path()).unwrap();

        assert!(matches!(
            workspace.resolve_file("../outside"),
            Err(ResolveError::OutsideWorkspace)
        ));
        assert!(matches!(
            workspace.resolve_file(temp.path()),
            Err(ResolveError::OutsideWorkspace)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn resolves_an_internal_symlink_to_its_canonical_resource() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("target"), "inside").unwrap();
        symlink("target", temp.path().join("link")).unwrap();
        let resolved = Workspace::open(temp.path())
            .unwrap()
            .resolve_file("link")
            .unwrap();

        assert_eq!(resolved.resource.value(), "target");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_outside_the_workspace() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        symlink(outside.path(), temp.path().join("escape")).unwrap();

        assert!(matches!(
            Workspace::open(temp.path()).unwrap().resolve_file("escape"),
            Err(ResolveError::OutsideWorkspace)
        ));
    }

    /// Runs everywhere, because the rewrite is string work and a Unix build
    /// that silently broke it would only be caught on Windows.
    #[test]
    fn a_verbatim_prefix_is_dropped_but_a_unc_share_is_left_alone() {
        assert_eq!(
            without_verbatim_prefix(r"\\?\C:\Users\runner\repo"),
            Some(r"C:\Users\runner\repo")
        );
        assert_eq!(without_verbatim_prefix(r"C:\Users\runner\repo"), None);
        assert_eq!(without_verbatim_prefix("/home/runner/repo"), None);
        assert_eq!(
            without_verbatim_prefix(r"\\?\UNC\server\share\repo"),
            None,
            "a share needs a different rewrite to stay usable"
        );
    }
}
