use arsy_kernel::domain::AgentId;
use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

const MAX_SNAPSHOT_FILES: usize = 100_000;
const MAX_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IsolationBackend {
    GitWorktree,
    CopiedSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceLease {
    pub owner: AgentId,
    pub source: PathBuf,
    pub view: PathBuf,
    pub base_revision: String,
    pub backend: IsolationBackend,
    pub mutable: bool,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeOutcome {
    Applied {
        commit: String,
    },
    Conflict {
        paths: Vec<PathBuf>,
        evidence: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Recovery {
    ReuseCommitted { owner: AgentId, revision: String },
    ReclaimUncommitted { owner: AgentId },
}

#[derive(Default)]
pub struct WorkspaceCoordinator {
    leases: BTreeMap<AgentId, WorkspaceLease>,
    readers: BTreeMap<(PathBuf, String), PathBuf>,
}

impl WorkspaceCoordinator {
    pub fn writer(
        &mut self,
        source: &Path,
        isolation_root: &Path,
        owner: AgentId,
        expires_at_ms: u64,
    ) -> Result<WorkspaceLease, WorkspaceError> {
        if self.leases.contains_key(&owner) {
            return Err(WorkspaceError::OwnerAlreadyHasWriter(owner));
        }
        let source = fs::canonicalize(source)?;
        fs::create_dir_all(isolation_root)?;
        let view = isolation_root.join(owner.to_string());
        let (base_revision, backend) = match git_revision(&source) {
            Ok(revision) => {
                git(
                    &source,
                    ["worktree", "add", "--detach", path_text(&view)?, &revision],
                )?;
                (revision, IsolationBackend::GitWorktree)
            }
            Err(WorkspaceError::NotGit) => {
                copy_snapshot(&source, &view)?;
                (
                    crate::edit::workspace_version(&source)?.0.to_string(),
                    IsolationBackend::CopiedSnapshot,
                )
            }
            Err(error) => return Err(error),
        };
        let lease = WorkspaceLease {
            owner,
            source,
            view,
            base_revision,
            backend,
            mutable: true,
            expires_at_ms,
        };
        self.leases.insert(owner, lease.clone());
        Ok(lease)
    }

    pub fn reader(
        &mut self,
        source: &Path,
        isolation_root: &Path,
        owner: AgentId,
    ) -> Result<WorkspaceLease, WorkspaceError> {
        let source = fs::canonicalize(source)?;
        let base = git_revision(&source).or_else(|error| match error {
            WorkspaceError::NotGit => Ok(crate::edit::workspace_version(&source)?.0.to_string()),
            other => Err(other),
        })?;
        let key = (source.clone(), base.clone());
        let view = if let Some(existing) = self.readers.get(&key) {
            existing.clone()
        } else {
            fs::create_dir_all(isolation_root)?;
            let view = isolation_root.join(format!("read-{base}"));
            if git_revision(&source).is_ok() {
                git(
                    &source,
                    ["worktree", "add", "--detach", path_text(&view)?, &base],
                )?;
            } else {
                copy_snapshot(&source, &view)?;
            }
            make_read_only(&view)?;
            self.readers.insert(key, view.clone());
            view
        };
        let backend = if git_revision(&source).is_ok() {
            IsolationBackend::GitWorktree
        } else {
            IsolationBackend::CopiedSnapshot
        };
        Ok(WorkspaceLease {
            owner,
            source,
            view,
            base_revision: base,
            backend,
            mutable: false,
            expires_at_ms: u64::MAX,
        })
    }

    pub fn recover_expired(&mut self, now_ms: u64) -> Result<Vec<Recovery>, WorkspaceError> {
        let owners: Vec<_> = self
            .leases
            .iter()
            .filter_map(|(owner, lease)| (lease.expires_at_ms <= now_ms).then_some(*owner))
            .collect();
        let mut output = Vec::new();
        for owner in owners {
            let lease = self.leases.remove(&owner).expect("selected above");
            let revision = git_revision(&lease.view).ok();
            if let Some(revision) = revision.filter(|revision| revision != &lease.base_revision) {
                output.push(Recovery::ReuseCommitted { owner, revision });
            } else {
                output.push(Recovery::ReclaimUncommitted { owner });
            }
        }
        Ok(output)
    }

    pub fn merge_git(
        &self,
        target: &Path,
        writer_revision: &str,
        policy_authorized: bool,
    ) -> Result<MergeOutcome, WorkspaceError> {
        if !policy_authorized {
            return Err(WorkspaceError::PolicyRequired);
        }
        if !matches!(writer_revision.len(), 40 | 64)
            || !writer_revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(WorkspaceError::InvalidRevision);
        }
        let output = Command::new("git")
            .args([
                "-C",
                path_text(target)?,
                "merge",
                "--no-edit",
                writer_revision,
            ])
            .output()?;
        if output.status.success() {
            return Ok(MergeOutcome::Applied {
                commit: git_revision(target)?,
            });
        }
        let conflicts = git_output(target, ["diff", "--name-only", "--diff-filter=U"])?
            .lines()
            .map(PathBuf::from)
            .collect();
        let evidence = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = git(target, ["merge", "--abort"]);
        Ok(MergeOutcome::Conflict {
            paths: conflicts,
            evidence,
        })
    }
}

fn git_revision(root: &Path) -> Result<String, WorkspaceError> {
    let output = Command::new("git")
        .args(["-C", path_text(root)?, "rev-parse", "HEAD"])
        .output()?;
    if !output.status.success() {
        return Err(WorkspaceError::NotGit);
    }
    Ok(String::from_utf8(output.stdout)
        .map_err(|error| WorkspaceError::Git(error.to_string()))?
        .trim()
        .to_owned())
}

fn git<const N: usize>(root: &Path, args: [&str; N]) -> Result<(), WorkspaceError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(WorkspaceError::Git(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

fn git_output<const N: usize>(root: &Path, args: [&str; N]) -> Result<String, WorkspaceError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|error| WorkspaceError::Git(error.to_string()))
    } else {
        Err(WorkspaceError::Git(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

fn copy_snapshot(source: &Path, destination: &Path) -> Result<(), WorkspaceError> {
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in walker(source) {
        let entry = entry.map_err(walk_error)?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|_| WorkspaceError::InvalidPath(entry.path().to_owned()))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(relative);
        let kind = entry
            .file_type()
            .ok_or_else(|| WorkspaceError::UnsupportedFile(entry.path().to_owned()))?;
        if kind.is_dir() {
            fs::create_dir_all(target)?;
        } else if kind.is_file() {
            files += 1;
            bytes = bytes
                .checked_add(entry.metadata().map_err(walk_error)?.len())
                .ok_or(WorkspaceError::SnapshotTooLarge)?;
            if files > MAX_SNAPSHOT_FILES || bytes > MAX_SNAPSHOT_BYTES {
                return Err(WorkspaceError::SnapshotTooLarge);
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), target)?;
        } else {
            return Err(WorkspaceError::UnsupportedFile(entry.path().to_owned()));
        }
    }
    Ok(())
}

fn make_read_only(root: &Path) -> Result<(), WorkspaceError> {
    for entry in walker(root) {
        let entry = entry.map_err(walk_error)?;
        let metadata = entry.metadata().map_err(walk_error)?;
        let mut permissions = metadata.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(entry.path(), permissions)?;
    }
    Ok(())
}

fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false)
        .build()
}

fn walk_error(error: ignore::Error) -> WorkspaceError {
    WorkspaceError::Io(io::Error::other(error))
}

fn path_text(path: &Path) -> Result<&str, WorkspaceError> {
    path.to_str()
        .ok_or_else(|| WorkspaceError::InvalidPath(path.to_owned()))
}

#[derive(Debug)]
pub enum WorkspaceError {
    OwnerAlreadyHasWriter(AgentId),
    PolicyRequired,
    NotGit,
    SnapshotTooLarge,
    UnsupportedFile(PathBuf),
    InvalidPath(PathBuf),
    InvalidRevision,
    Git(String),
    Edit(crate::edit::EditError),
    Io(io::Error),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OwnerAlreadyHasWriter(owner) => {
                write!(formatter, "agent {owner} already owns a mutable workspace")
            }
            Self::PolicyRequired => {
                formatter.write_str("workspace merge requires policy authorization")
            }
            Self::NotGit => formatter.write_str("workspace is not a Git repository"),
            Self::SnapshotTooLarge => {
                formatter.write_str("workspace snapshot exceeds bounded limits")
            }
            Self::UnsupportedFile(path) => write!(
                formatter,
                "snapshot refuses special file {}",
                path.display()
            ),
            Self::InvalidPath(path) => write!(formatter, "path is not UTF-8: {}", path.display()),
            Self::InvalidRevision => {
                formatter.write_str("writer revision must be a full hexadecimal object ID")
            }
            Self::Git(message) => formatter.write_str(message),
            Self::Edit(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl From<io::Error> for WorkspaceError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<crate::edit::EditError> for WorkspaceError {
    fn from(value: crate::edit::EditError) -> Self {
        Self::Edit(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_writers_are_unique_and_expired_uncommitted_views_are_reclaimed() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("file.txt"), "content").unwrap();
        let views = tempfile::tempdir().unwrap();
        let owner = AgentId::new();
        let mut coordinator = WorkspaceCoordinator::default();
        let lease = coordinator
            .writer(source.path(), views.path(), owner, 5)
            .unwrap();
        assert!(lease.mutable);
        assert_eq!(lease.backend, IsolationBackend::CopiedSnapshot);
        assert_eq!(
            fs::read_to_string(lease.view.join("file.txt")).unwrap(),
            "content"
        );
        assert!(matches!(
            coordinator.writer(source.path(), views.path(), owner, 10),
            Err(WorkspaceError::OwnerAlreadyHasWriter(id)) if id == owner
        ));
        assert_eq!(
            coordinator.recover_expired(5).unwrap(),
            vec![Recovery::ReclaimUncommitted { owner }]
        );
    }

    #[test]
    fn git_writers_use_worktrees_and_merge_conflicts_return_evidence() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), ["init", "--quiet"]).unwrap();
        git(source.path(), ["config", "user.name", "ARSY Test"]).unwrap();
        git(
            source.path(),
            ["config", "user.email", "arsy@example.invalid"],
        )
        .unwrap();
        fs::write(source.path().join("file.txt"), "base\n").unwrap();
        git(source.path(), ["add", "file.txt"]).unwrap();
        git(source.path(), ["commit", "--quiet", "-m", "base"]).unwrap();

        let views = tempfile::tempdir().unwrap();
        let mut coordinator = WorkspaceCoordinator::default();
        let lease = coordinator
            .writer(source.path(), views.path(), AgentId::new(), 10)
            .unwrap();
        assert_eq!(lease.backend, IsolationBackend::GitWorktree);
        fs::write(source.path().join("file.txt"), "target\n").unwrap();
        git(source.path(), ["commit", "--quiet", "-am", "target"]).unwrap();
        fs::write(lease.view.join("file.txt"), "writer\n").unwrap();
        git(&lease.view, ["commit", "--quiet", "-am", "writer"]).unwrap();
        let writer_revision = git_revision(&lease.view).unwrap();

        assert!(matches!(
            coordinator.merge_git(source.path(), &writer_revision, false),
            Err(WorkspaceError::PolicyRequired)
        ));
        let MergeOutcome::Conflict { paths, evidence } = coordinator
            .merge_git(source.path(), &writer_revision, true)
            .unwrap()
        else {
            panic!("divergent edits must conflict");
        };
        assert_eq!(paths, vec![PathBuf::from("file.txt")]);
        assert!(!evidence.is_empty());
    }
}
