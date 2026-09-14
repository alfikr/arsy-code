//! `repo.discover`: what kind of repository this workspace is, without a
//! shell command to guess it.
//!
//! `fs.list` and `search.files` can already find `Cargo.toml`, but the model
//! still has to know to look for it, and for `go.mod`, and for
//! `pnpm-workspace.yaml`, and to notice the `[workspace]` table rather than
//! the package one. This turns that inference into one call: the git root,
//! every manifest found within a bounded walk, the language each implies, and
//! the workspace members a manifest itself declares.

use crate::resource::Workspace;
use arsy_kernel::{
    artifact::ArtifactStore,
    capability::{CapabilityAction, CapabilityGrant},
    domain::ResourceRef,
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, OperationContract, OperationError,
        OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

/// How deep under the workspace root a manifest is still reported. A manifest
/// past this is a fixture or a vendored dependency, not part of the layout.
const MAX_DEPTH: usize = 3;
/// How many entries the walk inspects before giving up. Bounded so a
/// dependency-heavy workspace cannot turn discovery into a full tree walk.
const MAX_ENTRIES: usize = 5_000;

/// One manifest found in the walk, and the language it implies.
#[derive(Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Manifest {
    /// Workspace-relative path.
    pub path: String,
    pub language: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Discovery {
    /// Workspace-relative path to the git root, or the workspace root itself
    /// when no `.git` was found within it.
    pub git_root: Option<String>,
    pub manifests: Vec<Manifest>,
    /// Languages implied by the manifests found, sorted and deduplicated.
    pub languages: Vec<String>,
    /// Members a workspace-level manifest declares (Cargo `[workspace]`,
    /// npm/pnpm/yarn `workspaces`). Empty when nothing here is a monorepo.
    pub workspace_members: Vec<String>,
}

/// A manifest file name, the language it implies, and whether it can itself
/// declare a set of member packages.
const MANIFEST_KINDS: &[(&str, &str)] = &[
    ("Cargo.toml", "rust"),
    ("package.json", "javascript"),
    ("pyproject.toml", "python"),
    ("setup.py", "python"),
    ("go.mod", "go"),
    ("pom.xml", "java"),
    ("build.gradle", "java"),
    ("build.gradle.kts", "java"),
    ("Gemfile", "ruby"),
    ("composer.json", "php"),
];

fn language_of(file_name: &str) -> Option<&'static str> {
    MANIFEST_KINDS
        .iter()
        .find(|(name, _)| *name == file_name)
        .map(|(_, language)| *language)
}

/// The git root at or above `start`, as a path relative to `workspace_root`.
/// `None` when the walk leaves the workspace before finding one, which is the
/// ordinary case for a workspace opened outside any git checkout.
/// A workspace-relative path as it is reported.
///
/// `resolve_file` names a resource with forward slashes, so discovery answers
/// with the same spelling rather than handing a caller `crates\\one` on one
/// platform and `crates/one` on another. A backslash is a legal character in a
/// Unix file name, so only Windows rewrites.
fn reported(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text
    }
}

fn git_root(workspace_root: &Path) -> Option<PathBuf> {
    let mut candidate = workspace_root;
    loop {
        if candidate.join(".git").exists() {
            return Some(candidate.to_path_buf());
        }
        candidate = candidate.parent()?;
    }
}

/// Members a Cargo workspace manifest declares, resolved to workspace-relative
/// paths. Cargo allows a glob (`crates/*`); this only expands one level of
/// `*`, which is every layout this repository and the fixtures it targets
/// actually use.
fn cargo_members(root: &Path, manifest: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return Vec::new();
    };
    let Ok(document) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(members) = document
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(|members| members.as_array())
    else {
        return Vec::new();
    };
    let base = manifest.parent().unwrap_or(root);
    let mut resolved = Vec::new();
    for member in members.iter().filter_map(|value| value.as_str()) {
        if let Some(prefix) = member.strip_suffix("/*") {
            let Ok(entries) = std::fs::read_dir(base.join(prefix)) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.path().join("Cargo.toml").is_file() {
                    if let Ok(relative) = entry.path().strip_prefix(root) {
                        resolved.push(reported(relative));
                    }
                }
            }
        } else if let Ok(relative) = base.join(member).strip_prefix(root) {
            resolved.push(reported(relative));
        }
    }
    resolved.sort();
    resolved
}

/// Members an npm/pnpm/yarn `package.json` declares under `"workspaces"`.
fn npm_members(root: &Path, manifest: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return Vec::new();
    };
    let Ok(document) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let patterns = match document.get("workspaces") {
        Some(serde_json::Value::Array(patterns)) => patterns.clone(),
        Some(serde_json::Value::Object(object)) => object
            .get("packages")
            .cloned()
            .into_iter()
            .flat_map(|value| value.as_array().cloned().unwrap_or_default())
            .collect(),
        _ => return Vec::new(),
    };
    let base = manifest.parent().unwrap_or(root);
    let mut resolved = Vec::new();
    for pattern in patterns.iter().filter_map(|value| value.as_str()) {
        if let Some(prefix) = pattern.strip_suffix("/*") {
            let Ok(entries) = std::fs::read_dir(base.join(prefix)) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.path().join("package.json").is_file() {
                    if let Ok(relative) = entry.path().strip_prefix(root) {
                        resolved.push(reported(relative));
                    }
                }
            }
        } else if let Ok(relative) = base.join(pattern).strip_prefix(root) {
            resolved.push(reported(relative));
        }
    }
    resolved.sort();
    resolved
}

fn discover(workspace_root: &Path) -> Discovery {
    // `found` is at or above `workspace_root`; it is below only when the
    // workspace itself is the checkout root, the ordinary case. When the
    // workspace is a subdirectory of a larger checkout, `found` is an
    // ancestor and has no path relative to `workspace_root` at all — its own
    // absolute path is what is reported, not a workspace-relative one that
    // does not exist and must not be papered over as ".".
    let git_root = git_root(workspace_root).map(|found| {
        found
            .strip_prefix(workspace_root)
            .map(|relative| {
                if relative.as_os_str().is_empty() {
                    ".".to_owned()
                } else {
                    reported(relative)
                }
            })
            // A git root above the workspace is reported as the absolute path it
            // is, in the platform's own spelling: it is not a workspace-relative
            // path, so the one-spelling rule does not apply to it.
            .unwrap_or_else(|_| found.display().to_string())
    });

    let mut manifests = Vec::new();
    let mut languages = BTreeSet::new();
    let mut workspace_members = Vec::new();
    for (count, entry) in crate::resource::walk(workspace_root).flatten().enumerate() {
        if count >= MAX_ENTRIES {
            break;
        }
        // A manifest that is a symlink can resolve outside the workspace;
        // reading it (or a member glob resolved through it) would then read
        // whatever the link actually points at. Skipping it here, before
        // anything reads its content, is the one place that has to hold.
        if entry.file_type().is_some_and(|kind| kind.is_symlink()) {
            continue;
        }
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(workspace_root) else {
            continue;
        };
        if relative.components().count() > MAX_DEPTH {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(language) = language_of(file_name) else {
            continue;
        };
        languages.insert(language);
        manifests.push(Manifest {
            path: reported(relative),
            language: language.to_owned(),
        });
        match file_name {
            "Cargo.toml" => workspace_members.extend(cargo_members(workspace_root, path)),
            "package.json" => workspace_members.extend(npm_members(workspace_root, path)),
            _ => {}
        }
    }
    manifests.sort();
    workspace_members.sort();
    workspace_members.dedup();

    Discovery {
        git_root,
        manifests,
        languages: languages.into_iter().map(str::to_owned).collect(),
        workspace_members,
    }
}

pub struct DiscoveryExecutor {
    contract: OperationContract,
    workspace: PathBuf,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
}

impl DiscoveryExecutor {
    pub fn new(
        workspace: &Workspace,
        artifacts: Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            contract: OperationContract {
                kind: OperationKind::new("repo.discover").expect("static operation kind is valid"),
                input_schema: InputSchema {
                    required: Default::default(),
                    optional: Default::default(),
                    allow_extra: false,
                },
                actions: vec![CapabilityAction::FsRead],
                idempotency: Idempotency::Idempotent,
                reversible: true,
                concurrency: ConcurrencyRule::Parallel,
            },
            workspace: workspace.path().to_owned(),
            artifacts,
            retain_until_ms,
        })
    }
}

impl OperationExecutor for DiscoveryExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let result = discover(&self.workspace);
        let value = super::store(
            self.artifacts.as_ref(),
            &result,
            request.actor.clone(),
            self.retain_until_ms,
        )?;
        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: CapabilityAction::FsRead,
                resource: ResourceRef::new("workspace", ".")
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
            }],
            evidence: Vec::new(),
            state: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        artifact::{ArtifactReadLimits, FileArtifactStore},
        domain::{ArtifactId, OperationId, Principal},
    };

    fn discovery_at(root: &Path) -> Discovery {
        let workspace = Workspace::open(root).unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(root.join(".artifacts"), 0).unwrap());
        let executor = DiscoveryExecutor::new(&workspace, artifacts.clone(), 0);
        let request = OperationRequest {
            id: OperationId::new(),
            kind: executor.contract().kind.clone(),
            actor: Principal::User("test".into()),
            requirements: Vec::new(),
            input: serde_json::json!({}),
        };
        let outcome = executor.execute(&request, &[]).unwrap();
        let id: ArtifactId = outcome.value.unwrap().value().parse().unwrap();
        let bytes = artifacts
            .read(
                id,
                ArtifactReadLimits {
                    max_bytes: 1024 * 1024,
                    max_expansion_ratio: 1_000,
                },
            )
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn a_cargo_workspace_reports_its_git_root_language_and_members() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/one\", \"crates/two\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("crates/one")).unwrap();
        std::fs::write(
            root.join("crates/one/Cargo.toml"),
            "[package]\nname=\"one\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("crates/two")).unwrap();
        std::fs::write(
            root.join("crates/two/Cargo.toml"),
            "[package]\nname=\"two\"\n",
        )
        .unwrap();

        let discovery = discovery_at(root);
        assert_eq!(discovery.git_root.as_deref(), Some("."));
        assert_eq!(discovery.languages, vec!["rust"]);
        assert_eq!(
            discovery.workspace_members,
            vec!["crates/one".to_owned(), "crates/two".to_owned()]
        );
        assert!(discovery
            .manifests
            .iter()
            .any(|manifest| manifest.path == "Cargo.toml"));
    }

    #[test]
    fn a_plain_directory_with_no_git_reports_no_root() {
        let dir = tempfile::tempdir().unwrap();
        let discovery = discovery_at(dir.path());
        assert_eq!(discovery.git_root, None);
        assert!(discovery.manifests.is_empty());
    }

    #[test]
    fn an_npm_workspace_reports_its_members() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"root","private":true,"workspaces":["packages/*"]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("packages/app")).unwrap();
        std::fs::write(root.join("packages/app/package.json"), r#"{"name":"app"}"#).unwrap();

        let discovery = discovery_at(root);
        assert_eq!(discovery.languages, vec!["javascript"]);
        assert_eq!(discovery.workspace_members, vec!["packages/app".to_owned()]);
    }

    /// A workspace opened on a subdirectory of a larger checkout has a git
    /// root that is its own ancestor, not a descendant — `strip_prefix` fails,
    /// and the absolute path is what has to come back, not a workspace-
    /// relative "." that claims the workspace is the repository root.
    #[test]
    fn a_workspace_below_the_git_root_reports_the_real_root_not_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path();
        std::fs::create_dir_all(checkout.join(".git")).unwrap();
        let workspace = checkout.join("services/api");
        std::fs::create_dir_all(&workspace).unwrap();

        let discovery = discovery_at(&workspace);
        let expected = std::fs::canonicalize(checkout).unwrap();
        assert_eq!(
            discovery.git_root.as_deref(),
            Some(expected.to_str().unwrap())
        );
    }

    /// A manifest that is a symlink is never read: on some other path, that
    /// content could be outside the workspace entirely.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_manifest_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let secret = dir.path().parent().unwrap().join("outside-secret.toml");
        std::fs::write(&secret, "[workspace]\nmembers = [\"leak\"]\n").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("Cargo.toml")).unwrap();

        let discovery = discovery_at(root);
        assert!(
            discovery.manifests.is_empty(),
            "a symlinked manifest must not be read: {:?}",
            discovery.manifests
        );
        assert!(discovery.workspace_members.is_empty());
    }
}
