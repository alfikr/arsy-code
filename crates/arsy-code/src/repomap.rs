//! A compact description of the repository, kept between processes.
//!
//! # Why a map, and why it is persistent
//!
//! A model dropped into an unfamiliar repository spends its first several
//! rounds finding out what is in it: list a directory, read a file, grep for a
//! name, read another. That is work the harness has already done — the
//! [knowledge graph](crate::graph) walks the tree, parses what it can, and
//! records every declaration and import — and then throws away when the
//! process exits.
//!
//! So the graph is written to `.arsy/repo-map.json` and re-read at the start
//! of the next turn. Indexing is already incremental: a file whose bytes hash
//! to what was recorded is not reparsed, so refreshing a map of a large
//! repository costs one digest per file rather than one parse per file.
//!
//! # Why the projection is bounded and lossy
//!
//! The graph holds every declaration in the repository. A model's context
//! holds a few thousand tokens of it. A projection that tried to be complete
//! would either be refused by the provider or crowd out the task, so it is
//! explicitly a summary: the files with the most to say, their declarations,
//! and a note naming how much was left out and which tool finds the rest.
//!
//! # What is not in it
//!
//! Only languages the parser has a grammar for contribute declarations.
//! Everything else is still listed as a file — a map that silently omitted the
//! Python half of a repository would be worse than one that says "these files
//! exist, `search.text` reads them", which is what this does.

use crate::{
    graph::{EdgeKind, KnowledgeGraph, NodeKind},
    resource::Workspace,
};
use arsy_kernel::{
    artifact::ArtifactStore,
    capability::{CapabilityAction, CapabilityGrant},
    domain::ResourceRef,
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Where the map lives, relative to the workspace root.
pub const MAP_PATH: &str = ".arsy/repo-map.json";

/// What a projection may contribute to a turn.
///
/// About a thousand tokens: enough to name the shape of a repository, small
/// enough that a model still has room to work in one.
pub const DEFAULT_PROJECTION_BYTES: usize = 4 * 1024;

/// The stored form. Versioned because the graph's shape will change and a map
/// written by an older build must be discarded rather than misread.
#[derive(Deserialize, Serialize)]
struct Stored {
    version: u32,
    graph: KnowledgeGraph,
}

const STORED_VERSION: u32 = 1;

/// One workspace's map.
pub struct RepositoryMap {
    graph: KnowledgeGraph,
    path: PathBuf,
}

/// What a refresh did, for a caller that wants to say so.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Refreshed {
    pub files: usize,
    /// Files reparsed because their bytes changed. The measurement that says
    /// the refresh was incremental rather than a rebuild wearing its name.
    pub reindexed: usize,
    pub unchanged: usize,
    pub removed: usize,
    /// Files read but not parsed: too large, or a language with no grammar.
    pub unparsed: usize,
}

impl RepositoryMap {
    /// Read the stored map, or start an empty one.
    ///
    /// A map that cannot be read is a cache miss, not an error: the worst case
    /// is the cost of one full index, and refusing to start a turn because a
    /// cache file is corrupt would be a far worse failure than paying it.
    pub fn open(workspace_root: &Path) -> Self {
        let path = workspace_root.join(MAP_PATH);
        let graph = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Stored>(&bytes).ok())
            .filter(|stored| stored.version == STORED_VERSION)
            .map(|stored| stored.graph)
            .unwrap_or_default();
        Self { graph, path }
    }

    /// Bring the map up to date with the workspace and store it.
    ///
    /// Written back only when the index actually changed. A turn that calls
    /// `repo.map` on an untouched workspace should cost one digest per file
    /// and nothing else; re-serializing the whole graph to disk on every call
    /// would spend the most on exactly the repositories large enough to need
    /// an incremental index in the first place.
    pub fn refresh(
        &mut self,
        workspace: &Workspace,
    ) -> Result<Refreshed, crate::graph::GraphError> {
        let delta = self.graph.index(workspace)?;
        if !delta.is_empty() {
            self.save();
        }
        Ok(Refreshed {
            files: self.graph.file_count(),
            reindexed: delta.reindexed(),
            unchanged: delta.unchanged,
            removed: delta.removed.len(),
            unparsed: delta.unparsed,
        })
    }

    /// Write the map beside the session store.
    ///
    /// A write that fails is silent for the same reason a read that fails is:
    /// the map is a cache, and a workspace that cannot be written to should
    /// still be one a turn can run in.
    fn save(&self) {
        if let Some(parent) = self.path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        if let Ok(bytes) = serde_json::to_vec(&StoredRef {
            version: STORED_VERSION,
            graph: &self.graph,
        }) {
            let _ = std::fs::write(&self.path, bytes);
        }
    }

    /// A bounded summary of the repository, for a turn's context.
    ///
    /// Files are ordered by how much they declare, because a file with twenty
    /// declarations is more of an answer to "what is this repository" than one
    /// with none. What does not fit is counted rather than dropped silently.
    pub fn projection(&self, max_bytes: usize) -> String {
        let mut files: BTreeMap<PathBuf, FileSummary> = BTreeMap::new();
        for node in self.graph.nodes() {
            let Some(path) = &node.path else { continue };
            let entry = files.entry(path.clone()).or_default();
            match node.kind {
                NodeKind::Symbol => entry.declarations.push(node.name.clone()),
                NodeKind::File => {
                    entry.imports = self.graph.neighbours(&node.id, EdgeKind::Imports).len();
                }
                NodeKind::Module => {}
            }
        }

        let mut ranked: Vec<(PathBuf, FileSummary)> = files.into_iter().collect();
        // Most declarations first, path second so the order is stable and two
        // runs over the same tree produce the same map.
        ranked.sort_by(|left, right| {
            right
                .1
                .declarations
                .len()
                .cmp(&left.1.declarations.len())
                .then_with(|| left.0.cmp(&right.0))
        });

        let total = ranked.len();
        let mut text = format!("Repository map: {total} file(s) indexed.\n");
        let mut shown = 0;
        for (path, summary) in &ranked {
            let line = format!(
                "{}{}{}\n",
                path.display(),
                if summary.imports > 0 {
                    format!(" ({} import(s))", summary.imports)
                } else {
                    String::new()
                },
                if summary.declarations.is_empty() {
                    // Honest about the text tier: the file is here, its
                    // contents are findable, nothing parsed it.
                    String::new()
                } else {
                    format!(": {}", summary.declarations.join(", "))
                }
            );
            if text.len() + line.len() > max_bytes {
                break;
            }
            text.push_str(&line);
            shown += 1;
        }
        if shown < total {
            text.push_str(&format!(
                "[{} more file(s) not shown; find them with `search.files` and read them with \
                 `fs.read`.]\n",
                total - shown
            ));
        }
        text
    }

    pub fn file_count(&self) -> usize {
        self.graph.file_count()
    }
}

/// `repo.map`: refresh the map and return a bounded projection of it.
///
/// One operation rather than two — refresh and read — because a map the model
/// asked for and was given stale would be worse than no map: it would name
/// declarations that no longer exist, and a model cannot tell the difference.
pub struct MapExecutor {
    contract: OperationContract,
    workspace: PathBuf,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
}

/// What `repo.map` returns.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MapResult {
    #[serde(flatten)]
    pub refreshed: Refreshed,
    pub projection: String,
}

impl MapExecutor {
    pub fn new(
        workspace: &Workspace,
        artifacts: Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            contract: OperationContract {
                kind: OperationKind::new("repo.map").expect("static operation kind is valid"),
                input_schema: InputSchema {
                    required: Default::default(),
                    optional: [("max_bytes".to_owned(), JsonType::Number)]
                        .into_iter()
                        .collect(),
                    allow_extra: false,
                },
                // It reads the workspace and writes only its own cache under
                // `.arsy`, which is not the operator's content: a map is a
                // read of the repository, and is authorized as one.
                actions: vec![CapabilityAction::FsRead],
                idempotency: Idempotency::Idempotent,
                reversible: true,
                // The map is one file. Two refreshes racing would each write a
                // whole graph, and the loser's work would be thrown away.
                concurrency: ConcurrencyRule::ExclusiveGlobal,
            },
            workspace: workspace.path().to_owned(),
            artifacts,
            retain_until_ms,
        })
    }
}

impl OperationExecutor for MapExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let workspace = Workspace::open(&self.workspace)
            .map_err(|error| OperationError::Execution(error.to_string()))?;
        let mut map = RepositoryMap::open(&self.workspace);
        let refreshed = map
            .refresh(&workspace)
            .map_err(|error| OperationError::Execution(error.to_string()))?;
        let max_bytes = request
            .input
            .get("max_bytes")
            .and_then(serde_json::Value::as_u64)
            .and_then(|bytes| usize::try_from(bytes).ok())
            .unwrap_or(DEFAULT_PROJECTION_BYTES)
            .clamp(256, 64 * 1024);
        let result = MapResult {
            projection: map.projection(max_bytes),
            refreshed,
        };
        let value = crate::agent::store(
            self.artifacts.as_ref(),
            &result,
            request.actor.clone(),
            self.retain_until_ms,
        )?;
        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: CapabilityAction::FsRead,
                resource: ResourceRef::new("file", self.workspace.display().to_string())
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
            }],
            evidence: Vec::new(),
            state: None,
        })
    }
}

/// The borrowed half of [`Stored`], so writing does not clone the graph.
#[derive(Serialize)]
struct StoredRef<'a> {
    version: u32,
    graph: &'a KnowledgeGraph,
}

#[derive(Default)]
struct FileSummary {
    declarations: Vec<String>,
    imports: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        let directory = tempfile::tempdir().unwrap();
        for (path, body) in files {
            let full = directory.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        let workspace = Workspace::open(directory.path()).unwrap();
        (directory, workspace)
    }

    #[test]
    fn a_map_survives_the_process_and_reindexes_only_what_changed() {
        let (directory, workspace) = workspace(&[
            ("src/lib.rs", "pub mod util;\npub fn run() {}\n"),
            ("src/util.rs", "pub fn helper() {}\npub struct Config;\n"),
            ("README.md", "not code\n"),
        ]);

        let mut map = RepositoryMap::open(directory.path());
        let first = map.refresh(&workspace).unwrap();
        assert_eq!(
            first.reindexed, first.files,
            "a first pass indexes them all"
        );
        assert!(directory.path().join(MAP_PATH).is_file());

        // A second process opens the stored map and reparses nothing.
        let mut resumed = RepositoryMap::open(directory.path());
        assert_eq!(resumed.file_count(), first.files);
        let again = resumed.refresh(&workspace).unwrap();
        assert_eq!(
            again.reindexed, 0,
            "nothing changed, so nothing was reparsed: {again:?}"
        );
        assert_eq!(again.unchanged, first.files);

        // Nothing changed, so nothing was written either: the stored map is
        // byte-for-byte the one that was already there.
        let stored = std::fs::metadata(directory.path().join(MAP_PATH)).unwrap();
        let written_at = stored.modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        resumed.refresh(&workspace).unwrap();
        assert_eq!(
            std::fs::metadata(directory.path().join(MAP_PATH))
                .unwrap()
                .modified()
                .unwrap(),
            written_at,
            "an unchanged workspace must not rewrite the whole graph"
        );

        // One file changes; only that file is reindexed, and the map is stored.
        std::fs::write(
            directory.path().join("src/util.rs"),
            "pub fn helper() {}\npub struct Config;\npub fn added() {}\n",
        )
        .unwrap();
        let incremental = resumed.refresh(&workspace).unwrap();
        assert_eq!(incremental.reindexed, 1, "{incremental:?}");
        assert!(
            std::fs::metadata(directory.path().join(MAP_PATH))
                .unwrap()
                .modified()
                .unwrap()
                > written_at,
            "a change is written back"
        );
    }

    #[test]
    fn the_projection_is_bounded_and_says_what_it_left_out() {
        let mut files: Vec<(String, String)> = Vec::new();
        for index in 0..60 {
            files.push((
                format!("src/m{index}.rs"),
                format!("pub fn one{index}() {{}}\npub struct Two{index};\n"),
            ));
        }
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, body)| (path.as_str(), body.as_str()))
            .collect();
        let (directory, workspace) = workspace(&borrowed);

        let mut map = RepositoryMap::open(directory.path());
        map.refresh(&workspace).unwrap();

        // Deliberately tighter than the default, so the bound is exercised
        // rather than merely declared: sixty small files fit inside it.
        let budget = 600;
        let projection = map.projection(budget);
        assert!(
            projection.len() <= budget + 200,
            "a projection past its budget is the thing this bound exists to stop: {}",
            projection.len()
        );
        assert!(projection.contains("60 file(s) indexed"), "{projection}");
        assert!(
            map.projection(DEFAULT_PROJECTION_BYTES).len() > projection.len(),
            "a larger budget shows more of the repository"
        );
        assert!(
            projection.contains("not shown"),
            "what did not fit is counted, not silently dropped: {projection}"
        );
        assert!(
            projection.contains("search.files"),
            "and the model is told how to find it: {projection}"
        );
        assert!(projection.contains("one0"), "{projection}");
    }

    /// A language with no grammar still appears: a map that omitted it would
    /// read as "this repository has no Python in it".
    #[test]
    fn a_file_the_parser_cannot_read_is_still_on_the_map() {
        let (directory, workspace) = workspace(&[
            ("src/lib.rs", "pub fn run() {}\n"),
            ("scripts/deploy.py", "def deploy():\n    pass\n"),
        ]);
        let mut map = RepositoryMap::open(directory.path());
        map.refresh(&workspace).unwrap();

        let projection = map.projection(DEFAULT_PROJECTION_BYTES);
        assert!(projection.contains("deploy.py"), "{projection}");
        assert!(projection.contains("run"), "{projection}");
    }

    /// A corrupt cache costs one index, not a failed turn.
    #[test]
    fn an_unreadable_map_starts_empty_rather_than_failing() {
        let (directory, workspace) = workspace(&[("src/lib.rs", "pub fn run() {}\n")]);
        std::fs::create_dir_all(directory.path().join(".arsy")).unwrap();
        std::fs::write(directory.path().join(MAP_PATH), b"{not json").unwrap();

        let mut map = RepositoryMap::open(directory.path());
        assert_eq!(map.file_count(), 0);
        assert!(map.refresh(&workspace).unwrap().files > 0);
    }
}
