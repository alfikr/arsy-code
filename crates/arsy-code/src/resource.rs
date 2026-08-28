use arsy_kernel::domain::{ResourceRef, ResourceRefError};
use cap_std::{ambient_authority, fs::Dir};
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
}

/// A capability directory that confines all path resolution to one workspace.
pub struct Workspace {
    root: Dir,
}

impl Workspace {
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        Dir::open_ambient_dir(root, ambient_authority()).map(|root| Self { root })
    }

    pub fn resolve_file(&self, path: impl AsRef<Path>) -> Result<ResolvedFile, ResolveError> {
        let path = confined(path.as_ref())?;
        let canonical = self.root.canonicalize(&path)?;
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
    Io(io::Error),
    Resource(ResourceRefError),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideWorkspace => formatter.write_str("path escapes the workspace"),
            Self::EmptyPath => formatter.write_str("path must name a workspace file"),
            Self::NonUtf8 => formatter.write_str("canonical workspace path is not UTF-8"),
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

        assert!(Workspace::open(temp.path())
            .unwrap()
            .resolve_file("escape")
            .is_err());
    }
}
