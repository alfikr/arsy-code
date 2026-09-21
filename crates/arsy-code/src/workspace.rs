use arsy_kernel::domain::{AgentId, AttemptId, SessionId, TaskId};
use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, MutexGuard, OnceLock},
};

const MAX_SNAPSHOT_FILES: usize = 100_000;
const MAX_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Git's own administration of a repository is not concurrent: `worktree
/// add`, `worktree prune`, and `merge` all take the index and the ref store,
/// and two of them at once produce a lock error rather than a queue. Work
/// *inside* different views is untouched by this — only the bookkeeping that
/// reaches the shared repository is serialized.
fn admin() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What to do when the source has uncommitted work.
///
/// There is no "ignore it" arm on purpose: a view cut from a dirty tree that
/// silently dropped the operator's changes produces a diff against a base
/// nobody can reconstruct.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DirtyPolicy {
    /// Refuse to cut a view. The safe default: the operator decides what
    /// happens to their own uncommitted work.
    #[default]
    Refuse,
    /// Carry the uncommitted changes into the view as a patch, and say so.
    CarryPatch,
}

/// Who a view belongs to, as the parts a name can be built from without two
/// agents ever colliding.
#[derive(Clone, Copy, Debug)]
pub struct ViewOwner {
    pub session: SessionId,
    pub task: TaskId,
    pub attempt: AttemptId,
    pub agent: AgentId,
}

impl ViewOwner {
    /// A directory and ref name no other claim can produce.
    ///
    /// Every part is included because each one alone repeats: a task retried
    /// has two attempts, an agent may hold views in two sessions, and a name
    /// that collided would hand a retry the tree its predecessor left.
    pub fn slug(&self) -> String {
        let short = |id: &str| id.split('-').next().unwrap_or(id).to_owned();
        format!(
            "{}-{}-{}-{}",
            short(&self.session.to_string()),
            short(&self.task.to_string()),
            short(&self.attempt.to_string()),
            short(&self.agent.to_string()),
        )
    }

    /// The internal branch a writer commits on. Namespaced so it cannot be
    /// mistaken for anything a person made.
    pub fn branch(&self) -> String {
        format!("arsy/writer/{}", self.slug())
    }
}

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
    /// The internal branch a writer's commits land on, when the view is a
    /// Git worktree. `None` for readers and for copied snapshots.
    pub branch: Option<String>,
    /// Uncommitted work carried in from the source, when policy allowed it.
    pub carried_patch: Option<String>,
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

/// One writer's work, offered to the target.
///
/// A struct rather than arguments because the preflight reads most of it, and
/// a positional call with this many parts is how a caller ends up passing the
/// writer's base where the target's revision belongs.
pub struct IntegrationRequest<'a> {
    pub target: &'a Path,
    pub writer_revision: &'a str,
    /// What the writer started from, checked against where the target is now.
    pub base_revision: &'a str,
    pub changed_files: &'a [String],
    /// Paths already applied by an earlier integration in this round.
    pub already_integrated: &'a [String],
    /// Artifact ids for checks the writer ran. Empty means unvalidated, which
    /// is refused: a model saying it works is not a check.
    pub validation: &'a [String],
    /// What the writer says it did not settle.
    pub unresolved: &'a [String],
    /// Where the target is expected to be, when the caller has a revision it
    /// decided against.
    pub expect_target_revision: Option<&'a str>,
    pub allow_stale_base: bool,
    pub policy_authorized: bool,
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
        self.writer_for(
            source,
            isolation_root,
            &ViewOwner {
                session: SessionId::new(),
                task: TaskId::new(),
                attempt: AttemptId::new(),
                agent: owner,
            },
            expires_at_ms,
            DirtyPolicy::default(),
        )
    }

    /// Cut a mutable view for one attempt, on its own branch.
    ///
    /// The branch matters as much as the directory: a detached worktree loses
    /// what the writer committed as soon as the view is pruned, and an
    /// integrator reading a dangling object is reading something Git is free
    /// to collect.
    pub fn writer_for(
        &mut self,
        source: &Path,
        isolation_root: &Path,
        owner: &ViewOwner,
        expires_at_ms: u64,
        dirty: DirtyPolicy,
    ) -> Result<WorkspaceLease, WorkspaceError> {
        if self.leases.contains_key(&owner.agent) {
            return Err(WorkspaceError::OwnerAlreadyHasWriter(owner.agent));
        }
        let source = fs::canonicalize(source)?;
        fs::create_dir_all(isolation_root)?;
        let view = isolation_root.join(owner.slug());
        let _admin = admin();
        let (base_revision, backend, carried) = match git_revision(&source) {
            Ok(revision) => {
                let patch = match (uncommitted(&source)?, dirty) {
                    (None, _) => None,
                    (Some(_), DirtyPolicy::Refuse) => {
                        return Err(WorkspaceError::DirtySource(source));
                    }
                    (Some(patch), DirtyPolicy::CarryPatch) => Some(patch),
                };
                git(
                    &source,
                    [
                        "worktree",
                        "add",
                        "-b",
                        &owner.branch(),
                        path_text(&view)?,
                        &revision,
                    ],
                )?;
                if let Some(patch) = &patch {
                    apply_patch(&view, patch)?;
                }
                (revision, IsolationBackend::GitWorktree, patch)
            }
            Err(WorkspaceError::NotGit) => {
                copy_snapshot(&source, &view)?;
                (
                    crate::edit::workspace_version(&source)?.0.to_string(),
                    IsolationBackend::CopiedSnapshot,
                    None,
                )
            }
            Err(error) => return Err(error),
        };
        let lease = WorkspaceLease {
            owner: owner.agent,
            source,
            view,
            base_revision,
            backend,
            mutable: true,
            expires_at_ms,
            branch: Some(owner.branch()),
            carried_patch: carried,
        };
        self.leases.insert(owner.agent, lease.clone());
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
            let _admin = admin();
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
            branch: None,
            carried_patch: None,
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

    /// Apply one writer's work to the target, or say why it was not applied.
    ///
    /// Everything refusable is refused *before* the merge runs, because a
    /// merge that fails partway is a target left in conflict — and the
    /// invariant is that a refused integration leaves the target exactly as
    /// it was. The preflight is therefore not an optimisation: it is what
    /// makes "the target is unchanged" true.
    pub fn integrate(
        &self,
        request: &IntegrationRequest<'_>,
    ) -> Result<MergeOutcome, WorkspaceError> {
        if !request.policy_authorized {
            return Err(WorkspaceError::PolicyRequired);
        }
        if request.validation.is_empty() {
            return Err(WorkspaceError::Unvalidated);
        }
        if !request.unresolved.is_empty() {
            return Err(WorkspaceError::Unresolved(request.unresolved.join(", ")));
        }
        let _admin = admin();
        let target_revision = git_revision(request.target)?;
        if let Some(expected) = request.expect_target_revision {
            if expected != target_revision {
                return Err(WorkspaceError::TargetMoved {
                    expected: expected.to_owned(),
                    actual: target_revision,
                });
            }
        }
        if uncommitted(request.target)?.is_some() {
            return Err(WorkspaceError::DirtySource(request.target.to_owned()));
        }
        // A writer that started from a revision the target has since left is
        // proposing a change nobody validated against what is there now.
        if !request.allow_stale_base
            && !is_ancestor(request.target, request.base_revision, &target_revision)?
        {
            return Err(WorkspaceError::StaleBase {
                base: request.base_revision.to_owned(),
                target: target_revision,
            });
        }
        if let Some(path) = request
            .changed_files
            .iter()
            .find(|path| request.already_integrated.contains(&path.to_string()))
        {
            return Err(WorkspaceError::PathOverlap(path.to_string()));
        }
        self.merge_git(request.target, request.writer_revision, true)
    }

    /// Delete a view the coordinator cut, and forget the lease.
    ///
    /// Takes the branch with it only when it holds nothing the writer
    /// committed: a branch with work on it is evidence, and quarantining it
    /// is cheaper than explaining where it went.
    pub fn discard(&mut self, lease: &WorkspaceLease) -> Result<(), WorkspaceError> {
        let _admin = admin();
        self.leases.remove(&lease.owner);
        if lease.backend == IsolationBackend::GitWorktree {
            let _ = git(
                &lease.source,
                ["worktree", "remove", "--force", path_text(&lease.view)?],
            );
            let _ = git(&lease.source, ["worktree", "prune"]);
            if let Some(branch) = &lease.branch {
                // `-d` rather than `-D`: it deletes a branch that added
                // nothing and refuses one that did.
                let _ = git(&lease.source, ["branch", "-d", branch]);
            }
        } else if lease.view.exists() {
            fs::remove_dir_all(&lease.view)?;
        }
        Ok(())
    }

    /// Commit everything a writer left in its view, on its own branch.
    ///
    /// The harness does this rather than the writer, because committing is
    /// bookkeeping rather than engineering: a writer that had to run `git`
    /// would need process authority it was never given, and a writer that
    /// forgot would leave work an integrator cannot name a revision for.
    ///
    /// `None` when the view is unchanged — nothing to integrate is a fact,
    /// not a failure.
    pub fn commit_view(
        lease: &WorkspaceLease,
        message: &str,
    ) -> Result<Option<String>, WorkspaceError> {
        if lease.backend != IsolationBackend::GitWorktree {
            return Ok(None);
        }
        let _admin = admin();
        if uncommitted(&lease.view)?.is_none() {
            let head = git_revision(&lease.view)?;
            return Ok((head != lease.base_revision).then_some(head));
        }
        git(
            &lease.view,
            ["add", "--all", "--", ".", EXCLUDE_HARNESS_STATE],
        )?;
        git(
            &lease.view,
            [
                "-c",
                "user.name=ARSY",
                "-c",
                "user.email=arsy@localhost",
                "commit",
                "--quiet",
                "--no-verify",
                "-m",
                message,
            ],
        )?;
        Ok(Some(git_revision(&lease.view)?))
    }

    /// What one Git repository is, wherever it is checked out.
    ///
    /// The root commit, because a path is where a clone happens to live and a
    /// remote is a thing an operator can rename.
    pub fn repository_identity(source: &Path) -> Result<String, WorkspaceError> {
        Ok(git_output(source, ["rev-list", "--max-parents=0", "HEAD"])?
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned())
    }

    fn merge_git(
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

/// The source's uncommitted work as a patch, or `None` when it is clean.
///
/// Both halves: `diff HEAD` covers tracked changes, and untracked files are
/// included by intent-to-add so a new file is not silently left behind.
fn uncommitted(root: &Path) -> Result<Option<String>, WorkspaceError> {
    let untracked = git_output(
        root,
        [
            "ls-files",
            "--others",
            "--exclude-standard",
            "--",
            ".",
            EXCLUDE_HARNESS_STATE,
        ],
    )?;
    for path in untracked.lines().filter(|line| !line.trim().is_empty()) {
        git(root, ["add", "--intent-to-add", "--", path])?;
    }
    let patch = git_output(
        root,
        ["diff", "HEAD", "--binary", "--", ".", EXCLUDE_HARNESS_STATE],
    )?;
    Ok((!patch.trim().is_empty()).then_some(patch))
}

/// ARSY's own state lives in the workspace, so every run makes the tree
/// "dirty" by writing its event store, its artifacts, and the very views this
/// module cuts. That is the harness talking about itself, not the operator's
/// uncommitted work, and treating it as the latter would refuse every writer
/// in a repository ARSY has ever run in.
const EXCLUDE_HARNESS_STATE: &str = ":(exclude).arsy/**";

/// The same directory, as a path prefix.
pub const HARNESS_STATE: &str = ".arsy";

fn apply_patch(view: &Path, patch: &str) -> Result<(), WorkspaceError> {
    let file = view.join(".arsy-carried.patch");
    fs::write(&file, patch)?;
    let applied = git(view, ["apply", "--allow-empty", path_text(&file)?]);
    fs::remove_file(&file)?;
    applied
}

/// Whether `ancestor` is in `descendant`'s history.
fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool, WorkspaceError> {
    Ok(Command::new("git")
        .args([
            "-C",
            path_text(root)?,
            "merge-base",
            "--is-ancestor",
            ancestor,
            descendant,
        ])
        .output()?
        .status
        .success())
}

fn copy_snapshot(source: &Path, destination: &Path) -> Result<(), WorkspaceError> {
    // Made first rather than on the first entry copied: a workspace with
    // nothing in it is still a workspace, and a view that does not exist is
    // not one anybody can open.
    fs::create_dir_all(destination)?;
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in walker(source) {
        let entry = entry.map_err(walk_error)?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|_| WorkspaceError::InvalidPath(entry.path().to_owned()))?;
        // ARSY's own state, including the directory these views are cut
        // into. Copying it would copy the snapshot into itself.
        if relative.as_os_str().is_empty() || relative.starts_with(HARNESS_STATE) {
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
    /// Uncommitted work where the caller needs a known tree.
    DirtySource(PathBuf),
    /// The writer started from a revision the target has since left.
    StaleBase {
        base: String,
        target: String,
    },
    /// The target is not where the caller decided it was.
    TargetMoved {
        expected: String,
        actual: String,
    },
    /// A path an earlier integration in this round already applied.
    PathOverlap(String),
    /// No recorded check backs the change.
    Unvalidated,
    /// The writer says it left something unsettled.
    Unresolved(String),
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
            Self::DirtySource(path) => write!(
                formatter,
                "{} has uncommitted work; commit, stash, or allow it to be carried",
                path.display()
            ),
            Self::StaleBase { base, target } => write!(
                formatter,
                "the writer started from {base}, which is not in {target}'s history"
            ),
            Self::TargetMoved { expected, actual } => write!(
                formatter,
                "the target was {expected} when this was decided and is {actual} now"
            ),
            Self::PathOverlap(path) => {
                write!(formatter, "{path} was already changed by this round")
            }
            Self::Unvalidated => {
                formatter.write_str("integration requires recorded validation evidence")
            }
            Self::Unresolved(what) => write!(formatter, "the writer left {what} unresolved"),
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

    /// A repository with one commit, and the identity to address it by.
    fn repository() -> tempfile::TempDir {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), ["init", "--quiet"]).unwrap();
        git(source.path(), ["config", "user.name", "ARSY Test"]).unwrap();
        git(
            source.path(),
            ["config", "user.email", "arsy@example.invalid"],
        )
        .unwrap();
        // Windows runners default `core.autocrlf` to true, which rewrites
        // line endings on checkout. These tests compare file contents byte
        // for byte to say whether a writer's work arrived, and that question
        // has nothing to do with Git's line-ending policy.
        git(source.path(), ["config", "core.autocrlf", "false"]).unwrap();
        fs::write(source.path().join("file.txt"), "base\n").unwrap();
        git(source.path(), ["add", "file.txt"]).unwrap();
        git(source.path(), ["commit", "--quiet", "-m", "base"]).unwrap();
        source
    }

    fn owner() -> ViewOwner {
        ViewOwner {
            session: SessionId::new(),
            task: TaskId::new(),
            attempt: AttemptId::new(),
            agent: AgentId::new(),
        }
    }

    /// Commit `content` in a view and return the revision it produced.
    fn commit_in(view: &Path, content: &str) -> String {
        fs::write(view.join("file.txt"), content).unwrap();
        git(view, ["config", "user.name", "ARSY Test"]).unwrap();
        git(view, ["config", "user.email", "arsy@example.invalid"]).unwrap();
        git(view, ["commit", "--quiet", "-am", "writer"]).unwrap();
        git_revision(view).unwrap()
    }

    fn request<'a>(
        target: &'a Path,
        writer_revision: &'a str,
        base: &'a str,
        changed: &'a [String],
        validation: &'a [String],
    ) -> IntegrationRequest<'a> {
        IntegrationRequest {
            target,
            writer_revision,
            base_revision: base,
            changed_files: changed,
            already_integrated: &[],
            validation,
            unresolved: &[],
            expect_target_revision: None,
            allow_stale_base: false,
            policy_authorized: true,
        }
    }

    #[test]
    fn two_writers_work_in_separate_views_and_both_reach_the_target() {
        let source = repository();
        fs::write(source.path().join("second.txt"), "base\n").unwrap();
        git(source.path(), ["add", "second.txt"]).unwrap();
        git(source.path(), ["commit", "--quiet", "-m", "second"]).unwrap();
        let views = tempfile::tempdir().unwrap();
        let mut coordinator = WorkspaceCoordinator::default();

        let (first, second) = (owner(), owner());
        let one = coordinator
            .writer_for(source.path(), views.path(), &first, 10, DirtyPolicy::Refuse)
            .unwrap();
        let two = coordinator
            .writer_for(
                source.path(),
                views.path(),
                &second,
                10,
                DirtyPolicy::Refuse,
            )
            .unwrap();
        assert_ne!(one.view, two.view, "each writer gets its own tree");
        assert_ne!(one.branch, two.branch, "and its own branch");

        let base = one.base_revision.clone();
        let one_head = commit_in(&one.view, "from one\n");
        fs::write(two.view.join("second.txt"), "from two\n").unwrap();
        git(&two.view, ["config", "user.name", "ARSY Test"]).unwrap();
        git(&two.view, ["config", "user.email", "arsy@example.invalid"]).unwrap();
        git(&two.view, ["commit", "--quiet", "-am", "writer two"]).unwrap();
        let two_head = git_revision(&two.view).unwrap();

        // An integrator applies both, in order, to the recorded target.
        let checks = vec!["artifact-check".to_owned()];
        let first_files = vec!["file.txt".to_owned()];
        assert!(matches!(
            coordinator
                .integrate(&request(
                    source.path(),
                    &one_head,
                    &base,
                    &first_files,
                    &checks
                ))
                .unwrap(),
            MergeOutcome::Applied { .. }
        ));
        let second_files = vec!["second.txt".to_owned()];
        assert!(matches!(
            coordinator
                .integrate(&IntegrationRequest {
                    already_integrated: &first_files,
                    ..request(source.path(), &two_head, &base, &second_files, &checks)
                })
                .unwrap(),
            MergeOutcome::Applied { .. }
        ));
        assert_eq!(
            fs::read_to_string(source.path().join("file.txt")).unwrap(),
            "from one\n"
        );
        assert_eq!(
            fs::read_to_string(source.path().join("second.txt")).unwrap(),
            "from two\n"
        );
    }

    #[test]
    fn integration_refuses_before_it_touches_the_target() {
        let source = repository();
        let views = tempfile::tempdir().unwrap();
        let mut coordinator = WorkspaceCoordinator::default();
        let lease = coordinator
            .writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::Refuse,
            )
            .unwrap();
        let base = lease.base_revision.clone();
        let head = commit_in(&lease.view, "writer\n");
        let before = git_revision(source.path()).unwrap();
        let changed = vec!["file.txt".to_owned()];
        let checks = vec!["artifact-check".to_owned()];

        // Each refusal is its own reason, and none of them moves the target.
        assert!(matches!(
            coordinator.integrate(&IntegrationRequest {
                policy_authorized: false,
                ..request(source.path(), &head, &base, &changed, &checks)
            }),
            Err(WorkspaceError::PolicyRequired)
        ));
        assert!(matches!(
            coordinator.integrate(&request(source.path(), &head, &base, &changed, &[])),
            Err(WorkspaceError::Unvalidated)
        ));
        assert!(matches!(
            coordinator.integrate(&IntegrationRequest {
                unresolved: &["a failing test".to_owned()],
                ..request(source.path(), &head, &base, &changed, &checks)
            }),
            Err(WorkspaceError::Unresolved(_))
        ));
        assert!(matches!(
            coordinator.integrate(&IntegrationRequest {
                expect_target_revision: Some("0".repeat(40).as_str()),
                ..request(source.path(), &head, &base, &changed, &checks)
            }),
            Err(WorkspaceError::TargetMoved { .. })
        ));
        assert!(matches!(
            coordinator.integrate(&IntegrationRequest {
                already_integrated: &changed,
                ..request(source.path(), &head, &base, &changed, &checks)
            }),
            Err(WorkspaceError::PathOverlap(_))
        ));
        assert!(matches!(
            coordinator.integrate(&request(
                source.path(),
                &head,
                &"0".repeat(40),
                &changed,
                &checks
            )),
            Err(WorkspaceError::StaleBase { .. })
        ));
        assert_eq!(git_revision(source.path()).unwrap(), before);
        assert_eq!(
            fs::read_to_string(source.path().join("file.txt")).unwrap(),
            "base\n"
        );
    }

    #[test]
    fn a_dirty_source_is_refused_or_carried_and_never_silently_dropped() {
        let source = repository();
        fs::write(source.path().join("file.txt"), "uncommitted\n").unwrap();
        let views = tempfile::tempdir().unwrap();
        let mut coordinator = WorkspaceCoordinator::default();
        assert!(matches!(
            coordinator.writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::Refuse
            ),
            Err(WorkspaceError::DirtySource(_))
        ));
        let carried = coordinator
            .writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::CarryPatch,
            )
            .unwrap();
        assert!(carried.carried_patch.is_some());
        assert_eq!(
            fs::read_to_string(carried.view.join("file.txt")).unwrap(),
            "uncommitted\n",
            "the operator's work reaches the view rather than vanishing"
        );
    }

    #[test]
    fn a_discarded_view_leaves_nothing_behind_but_keeps_committed_work() {
        let source = repository();
        let views = tempfile::tempdir().unwrap();
        let mut coordinator = WorkspaceCoordinator::default();
        let idle = coordinator
            .writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::Refuse,
            )
            .unwrap();
        coordinator.discard(&idle).unwrap();
        assert!(!idle.view.exists());
        assert!(!git_output(source.path(), ["branch", "--list"])
            .unwrap()
            .contains(idle.branch.as_deref().unwrap()));

        let worked = coordinator
            .writer_for(
                source.path(),
                views.path(),
                &owner(),
                10,
                DirtyPolicy::Refuse,
            )
            .unwrap();
        let head = commit_in(&worked.view, "kept\n");
        coordinator.discard(&worked).unwrap();
        assert!(!worked.view.exists());
        // The branch survives, because it is the only handle on what the
        // writer committed.
        assert!(git_output(source.path(), ["branch", "--list"])
            .unwrap()
            .contains(worked.branch.as_deref().unwrap()));
        assert!(git_output(source.path(), ["cat-file", "-t", &head]).is_ok());
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
