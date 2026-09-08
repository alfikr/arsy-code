//! The installed plugin registry, and refresh at a turn boundary.
//!
//! See `docs/19-plugin-extension-system.md`. Installation records what an
//! operator approved; every later load is checked against that record. Two
//! rules do the work:
//!
//! * **Refresh cannot widen capability.** A manifest whose requested
//!   capabilities exceed the grant recorded at install is rejected and the
//!   previous version stays loaded. Without this, refreshing would be the way
//!   around the approval an update needs.
//! * **Refresh lands between turns, never inside one.** A staged set is
//!   committed at a turn boundary, so a turn finishes with the extension set it
//!   began with and no hook or skill changes the rules mid-application.

use arsy_kernel::capability::{CapabilityAction, ResourcePattern};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

/// The file an installed plugin is described by.
pub const MANIFEST_FILE: &str = "plugin.toml";
/// Where a workspace keeps what it has installed.
pub const PLUGIN_DIRECTORY: &str = ".arsy/plugins";
/// The approval recorded at install time, beside the manifest it approved.
pub const GRANT_FILE: &str = "grant.json";
/// What a turn boundary last committed, `id -> version`. A dotfile so it never
/// collides with a plugin id.
pub const LOADED_FILE: &str = ".loaded.json";

/// The only manifest schema this build reads.
pub const MANIFEST_VERSION: i64 = 1;

/// One requested capability: an action over a resource pattern.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Request {
    pub action: CapabilityAction,
    pub pattern: String,
}

impl Request {
    /// `fs.read:workspace/**`, as the manifest writes it.
    pub fn parse(value: &str) -> Result<Self, PluginError> {
        let (action, pattern) = value
            .split_once(':')
            .ok_or_else(|| PluginError::Manifest(format!("`{value}` must be `<action>:<glob>`")))?;
        let action: CapabilityAction = serde_json::from_value(Value::String(action.to_owned()))
            .map_err(|_| PluginError::Manifest(format!("`{action}` is not a capability")))?;
        // Compiled here so a pattern that cannot be evaluated is refused at
        // install rather than at the moment it would have been enforced.
        ResourcePattern::new("plugin", pattern)
            .map_err(|error| PluginError::Manifest(format!("`{pattern}`: {error}")))?;
        Ok(Self {
            action,
            pattern: pattern.to_owned(),
        })
    }
}

impl fmt::Display for Request {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.action, self.pattern)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Manifest {
    pub id: String,
    pub version: String,
    pub entrypoint: String,
    /// Host API range the plugin was built against.
    pub api: String,
    pub capabilities: BTreeSet<Request>,
}

impl Manifest {
    pub fn parse(raw: &str) -> Result<Self, PluginError> {
        let table: toml::Table = raw
            .parse()
            .map_err(|error: toml::de::Error| PluginError::Manifest(error.message().to_owned()))?;
        match table
            .get("manifest_version")
            .and_then(toml::Value::as_integer)
        {
            Some(MANIFEST_VERSION) => {}
            Some(other) => {
                return Err(PluginError::Manifest(format!(
                    "unsupported manifest_version {other}"
                )))
            }
            None => {
                return Err(PluginError::Manifest(
                    "requires `manifest_version = 1`".to_owned(),
                ))
            }
        }
        let string = |key: &str| -> Result<String, PluginError> {
            table
                .get(key)
                .and_then(toml::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| PluginError::Manifest(format!("`{key}` is required")))
        };
        let entrypoint = string("entrypoint")?;
        // An entrypoint is opened relative to the plugin's own directory, so a
        // traversal in it would read a file the operator never installed.
        if Path::new(&entrypoint)
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(PluginError::Manifest(format!(
                "`entrypoint` must be a plain relative file name, not `{entrypoint}`"
            )));
        }
        let mut capabilities = BTreeSet::new();
        for requested in table
            .get("capabilities")
            .and_then(toml::Value::as_array)
            .unwrap_or(&Vec::new())
        {
            let requested = requested.as_str().ok_or_else(|| {
                PluginError::Manifest("`capabilities` entries must be strings".to_owned())
            })?;
            capabilities.insert(Request::parse(requested)?);
        }
        Ok(Self {
            id: string("id")?,
            version: string("version")?,
            entrypoint,
            api: string("api")?,
            capabilities,
        })
    }

    /// The distinct actions, which is what the WASM host checks a grant against.
    pub fn actions(&self) -> BTreeSet<CapabilityAction> {
        self.capabilities
            .iter()
            .map(|request| request.action)
            .collect()
    }
}

/// What an operator approved when the plugin was installed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, serde::Deserialize)]
pub struct Grant {
    pub granted: BTreeSet<String>,
    pub approved_version: String,
    pub approved_at_ms: u64,
}

impl Grant {
    pub fn for_manifest(manifest: &Manifest, now_ms: u64) -> Self {
        Self {
            granted: manifest
                .capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            approved_version: manifest.version.clone(),
            approved_at_ms: now_ms,
        }
    }

    /// Whether this grant already covers everything `manifest` asks for.
    pub fn covers(&self, manifest: &Manifest) -> bool {
        manifest
            .capabilities
            .iter()
            .all(|request| self.granted.contains(&request.to_string()))
    }

    /// What a manifest asks for beyond this grant, in a stable order.
    pub fn excess(&self, manifest: &Manifest) -> Vec<String> {
        manifest
            .capabilities
            .iter()
            .map(ToString::to_string)
            .filter(|request| !self.granted.contains(request))
            .collect()
    }
}

/// One plugin as it exists on disk.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Installed {
    pub manifest: Manifest,
    pub grant: Grant,
    pub directory: PathBuf,
    /// A signature establishes publisher identity, not safety. This build reads
    /// none, so the state is reported rather than implied.
    pub signature: &'static str,
}

impl Installed {
    /// Whether this plugin may be loaded: its manifest asks for nothing beyond
    /// what was approved.
    pub fn loadable(&self) -> bool {
        self.grant.covers(&self.manifest)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginError {
    Manifest(String),
    NotInstalled(String),
    AlreadyInstalled(String),
    /// The source does not contain what a plugin needs.
    Source(String),
    Io(String),
}

impl fmt::Display for PluginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(detail) => write!(formatter, "{MANIFEST_FILE}: {detail}"),
            Self::NotInstalled(id) => write!(formatter, "no plugin `{id}` is installed"),
            Self::AlreadyInstalled(id) => write!(formatter, "plugin `{id}` is already installed"),
            Self::Source(detail) => write!(formatter, "{detail}"),
            Self::Io(detail) => write!(formatter, "{detail}"),
        }
    }
}

impl std::error::Error for PluginError {}

fn io(error: std::io::Error) -> PluginError {
    PluginError::Io(error.to_string())
}

/// Everything a listing found: what parsed, and what did not with its reason.
pub type Listing = (Vec<Installed>, Vec<(String, PluginError)>);

/// The plugins one workspace has installed.
pub struct Registry {
    root: PathBuf,
}

impl Registry {
    pub fn open(workspace: &Path) -> Self {
        Self {
            root: workspace.join(PLUGIN_DIRECTORY),
        }
    }

    pub fn directory(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// Everything installed, in id order. A directory that does not parse is
    /// reported against itself rather than failing the whole listing: one bad
    /// plugin must not hide the rest.
    pub fn list(&self) -> Result<Listing, PluginError> {
        let mut installed = Vec::new();
        let mut broken = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((installed, broken))
            }
            Err(error) => return Err(io(error)),
        };
        let mut directories: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io)?;
            if entry.file_type().map_err(io)?.is_dir() {
                directories.push(entry.path());
            }
        }
        directories.sort();
        for directory in directories {
            let name = directory
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned();
            match self.read(&directory) {
                Ok(plugin) => installed.push(plugin),
                Err(error) => broken.push((name, error)),
            }
        }
        Ok((installed, broken))
    }

    /// What was in force when a turn boundary last committed a refresh.
    ///
    /// An absent file means nothing has been committed yet, which is different
    /// from "everything installed is loaded": a plugin copied into the registry
    /// by hand is not loaded until a refresh adopts it.
    pub fn loaded_versions(&self) -> Result<BTreeMap<String, String>, PluginError> {
        match std::fs::read_to_string(self.root.join(LOADED_FILE)) {
            Ok(body) => {
                serde_json::from_str(&body).map_err(|error| PluginError::Io(error.to_string()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(io(error)),
        }
    }

    pub fn record_loaded(&self, loaded: &BTreeMap<String, String>) -> Result<(), PluginError> {
        std::fs::create_dir_all(&self.root).map_err(io)?;
        std::fs::write(
            self.root.join(LOADED_FILE),
            serde_json::to_vec_pretty(loaded)
                .map_err(|error| PluginError::Io(error.to_string()))?,
        )
        .map_err(io)
    }

    pub fn get(&self, id: &str) -> Result<Installed, PluginError> {
        let directory = self.directory(id);
        if !directory.is_dir() {
            return Err(PluginError::NotInstalled(id.to_owned()));
        }
        self.read(&directory)
    }

    fn read(&self, directory: &Path) -> Result<Installed, PluginError> {
        let manifest =
            Manifest::parse(&std::fs::read_to_string(directory.join(MANIFEST_FILE)).map_err(io)?)?;
        let grant: Grant =
            serde_json::from_str(&std::fs::read_to_string(directory.join(GRANT_FILE)).map_err(io)?)
                .map_err(|error| PluginError::Manifest(error.to_string()))?;
        Ok(Installed {
            manifest,
            grant,
            directory: directory.to_owned(),
            signature: "absent",
        })
    }

    /// Read a manifest from an install source without installing it, so the
    /// caller can show what it asks for before anything is copied.
    pub fn inspect_source(source: &Path) -> Result<(Manifest, PathBuf), PluginError> {
        let manifest_path = source.join(MANIFEST_FILE);
        if !manifest_path.is_file() {
            return Err(PluginError::Source(format!(
                "{} has no {MANIFEST_FILE}",
                source.display()
            )));
        }
        let manifest = Manifest::parse(&std::fs::read_to_string(&manifest_path).map_err(io)?)?;
        let entrypoint = source.join(&manifest.entrypoint);
        if !entrypoint.is_file() {
            return Err(PluginError::Source(format!(
                "the manifest names `{}`, which is not in {}",
                manifest.entrypoint,
                source.display()
            )));
        }
        Ok((manifest, entrypoint))
    }

    /// Copy a source into the registry and record the approval.
    ///
    /// `approved` is what the operator agreed to; a manifest asking for more is
    /// refused here rather than at load, so a partial install never exists.
    pub fn install(
        &self,
        source: &Path,
        approved: &Grant,
        now_ms: u64,
    ) -> Result<Installed, PluginError> {
        let (manifest, entrypoint) = Self::inspect_source(source)?;
        if !approved.covers(&manifest) {
            return Err(PluginError::Manifest(format!(
                "the approval does not cover {}",
                approved.excess(&manifest).join(", ")
            )));
        }
        let directory = self.directory(&manifest.id);
        if directory.exists() {
            return Err(PluginError::AlreadyInstalled(manifest.id.clone()));
        }
        std::fs::create_dir_all(&directory).map_err(io)?;
        std::fs::copy(source.join(MANIFEST_FILE), directory.join(MANIFEST_FILE)).map_err(io)?;
        std::fs::copy(&entrypoint, directory.join(&manifest.entrypoint)).map_err(io)?;
        let grant = Grant {
            approved_at_ms: now_ms,
            ..approved.clone()
        };
        std::fs::write(
            directory.join(GRANT_FILE),
            serde_json::to_vec_pretty(&grant)
                .map_err(|error| PluginError::Io(error.to_string()))?,
        )
        .map_err(io)?;
        Ok(Installed {
            manifest,
            grant,
            directory,
            signature: "absent",
        })
    }

    /// Uninstall and revoke. Removing the grant with the plugin is the point:
    /// reinstalling later asks for approval again.
    pub fn remove(&self, id: &str) -> Result<Installed, PluginError> {
        let installed = self.get(id)?;
        std::fs::remove_dir_all(&installed.directory).map_err(io)?;
        Ok(installed)
    }
}

/// The extension set a turn runs with, and the one staged to replace it.
///
/// Refresh re-reads sources and stages the result; nothing changes until a turn
/// boundary commits it. A staged entry that asks for more than was approved is
/// rejected, and the previously loaded version stays.
pub struct ExtensionSet {
    /// `id -> version` actually in force. A version rather than a manifest,
    /// because the loaded set outlives the process that loaded it: a source can
    /// be edited while nothing is running, and the only thing that survives to
    /// be compared against is what was last committed.
    loaded: BTreeMap<String, String>,
    staged: Option<Refresh>,
}

/// What one refresh found. Reported whether or not it is committed, which is
/// what `--dry-run` prints and what the committed event records.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Refresh {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    /// `id -> why`, for a source that was read but not adopted.
    pub rejected: BTreeMap<String, String>,
    #[serde(skip)]
    next: BTreeMap<String, Installed>,
}

impl Refresh {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }

    /// The plugins this refresh would put in force, for a host that has to
    /// load their modules once it is committed.
    pub const fn plugins(&self) -> &BTreeMap<String, Installed> {
        &self.next
    }

    pub fn report(&self) -> Value {
        json!({
            "added": self.added,
            "updated": self.updated,
            "removed": self.removed,
            "rejected": self.rejected,
        })
    }
}

impl ExtensionSet {
    /// Start from what was last committed, as `Registry::loaded_versions`
    /// reports it.
    pub fn new(loaded: BTreeMap<String, String>) -> Self {
        Self {
            loaded,
            staged: None,
        }
    }

    /// `id -> version` currently in force.
    pub const fn loaded(&self) -> &BTreeMap<String, String> {
        &self.loaded
    }

    pub fn pending(&self) -> Option<&Refresh> {
        self.staged.as_ref()
    }

    /// Re-read the registry and stage the result.
    ///
    /// `only` narrows the refresh to one plugin, which is what
    /// `arsy plugin refresh <ID>` does; everything else keeps the version it
    /// already has.
    pub fn refresh(
        &mut self,
        registry: &Registry,
        only: Option<&str>,
    ) -> Result<&Refresh, PluginError> {
        let (found, broken) = registry.list()?;
        let mut refresh = Refresh::default();
        for (id, error) in broken {
            // A malformed source never leaves the host with no extensions: the
            // failure is reported against the source that caused it and the
            // previously loaded version stays.
            refresh.rejected.insert(id, error.to_string());
        }
        let mut seen = BTreeSet::new();
        for plugin in found {
            let id = plugin.manifest.id.clone();
            if only.is_some_and(|only| only != id) {
                seen.insert(id);
                continue;
            }
            seen.insert(id.clone());
            if !plugin.loadable() {
                refresh.rejected.insert(
                    id,
                    format!(
                        "requests {} beyond what was approved; re-install to approve it",
                        plugin.grant.excess(&plugin.manifest).join(", ")
                    ),
                );
                continue;
            }
            match self.loaded.get(&id) {
                None => refresh.added.push(id.clone()),
                Some(version) if *version != plugin.manifest.version => {
                    refresh.updated.push(id.clone());
                }
                Some(_) => {}
            }
            refresh.next.insert(id, plugin);
        }
        for id in self.loaded.keys() {
            // A plugin outside a narrowed refresh keeps the version it has;
            // only a full refresh can conclude that one is gone.
            if only.is_some_and(|only| only != id) {
                continue;
            }
            if !seen.contains(id) {
                refresh.removed.push(id.clone());
            }
        }
        self.staged = Some(refresh);
        Ok(self.staged.as_ref().expect("just staged"))
    }

    /// Adopt a staged refresh. Only a turn boundary may call this: inside a
    /// turn, the set an agent started with is the set it finishes with.
    pub fn commit_at_turn_boundary(&mut self) -> Option<Refresh> {
        let staged = self.staged.take()?;
        for id in &staged.removed {
            self.loaded.remove(id);
        }
        for (id, plugin) in &staged.next {
            self.loaded
                .insert(id.clone(), plugin.manifest.version.clone());
        }
        Some(staged)
    }

    /// Drop a staged refresh without adopting it.
    pub fn discard(&mut self) -> Option<Refresh> {
        self.staged.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
manifest_version = 1
id = "example.review"
version = "1.2.0"
entrypoint = "plugin.wasm"
api = ">=1,<2"
capabilities = ["fs.read:workspace/**"]
"#;

    fn source(directory: &Path, manifest: &str) -> PathBuf {
        let source = directory.join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join(MANIFEST_FILE), manifest).unwrap();
        std::fs::write(source.join("plugin.wasm"), b"\0asm").unwrap();
        source
    }

    #[test]
    fn a_manifest_is_parsed_or_refused_with_a_reason() {
        let manifest = Manifest::parse(MANIFEST).unwrap();
        assert_eq!(manifest.id, "example.review");
        assert_eq!(manifest.actions(), [CapabilityAction::FsRead].into());
        assert_eq!(
            manifest.capabilities.iter().next().unwrap().to_string(),
            "fs.read:workspace/**"
        );

        for refused in [
            "id = \"x\"\n",
            "manifest_version = 2\nid = \"x\"\n",
            "manifest_version = 1\nversion = \"1\"\nentrypoint = \"p\"\napi = \"1\"\n",
            // An entrypoint that climbs out of its own directory.
            "manifest_version = 1\nid=\"x\"\nversion=\"1\"\napi=\"1\"\nentrypoint=\"../etc/passwd\"\n",
            // A capability that names no action, and one that is not a capability.
            "manifest_version = 1\nid=\"x\"\nversion=\"1\"\napi=\"1\"\nentrypoint=\"p\"\ncapabilities=[\"fs.read\"]\n",
            "manifest_version = 1\nid=\"x\"\nversion=\"1\"\napi=\"1\"\nentrypoint=\"p\"\ncapabilities=[\"fs.obliterate:**\"]\n",
        ] {
            assert!(Manifest::parse(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn install_records_the_approval_and_remove_revokes_it() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path();
        let source = source(workspace, MANIFEST);
        let registry = Registry::open(workspace);

        let (manifest, _) = Registry::inspect_source(&source).unwrap();
        // Installing with less than the manifest asks for is refused outright.
        assert!(registry
            .install(
                &source,
                &Grant {
                    granted: BTreeSet::new(),
                    approved_version: manifest.version.clone(),
                    approved_at_ms: 0,
                },
                1,
            )
            .is_err());
        assert!(
            !registry.directory(&manifest.id).exists(),
            "no partial install"
        );

        let approved = Grant::for_manifest(&manifest, 1);
        let installed = registry.install(&source, &approved, 7).unwrap();
        assert_eq!(installed.grant.approved_at_ms, 7);
        assert!(installed.loadable());
        assert_eq!(installed.signature, "absent");
        assert_eq!(
            registry.install(&source, &approved, 8),
            Err(PluginError::AlreadyInstalled("example.review".to_owned()))
        );

        let (listed, broken) = registry.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(broken.is_empty());
        assert_eq!(registry.get("example.review").unwrap().manifest, manifest);

        registry.remove("example.review").unwrap();
        assert_eq!(
            registry.get("example.review"),
            Err(PluginError::NotInstalled("example.review".to_owned()))
        );
        assert!(registry.list().unwrap().0.is_empty());
    }

    #[test]
    fn refresh_cannot_widen_capability_and_lands_at_a_turn_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path();
        let source = source(workspace, MANIFEST);
        let registry = Registry::open(workspace);
        let (manifest, _) = Registry::inspect_source(&source).unwrap();
        let installed = registry
            .install(&source, &Grant::for_manifest(&manifest, 1), 1)
            .unwrap();

        let mut set = ExtensionSet::new([("example.review".to_owned(), "1.2.0".to_owned())].into());
        assert_eq!(set.loaded().len(), 1);

        // The manifest on disk grows a capability the grant does not cover.
        std::fs::write(
            installed.directory.join(MANIFEST_FILE),
            MANIFEST.replace(
                "capabilities = [\"fs.read:workspace/**\"]",
                "capabilities = [\"fs.read:workspace/**\", \"process.exec:**\"]",
            ),
        )
        .unwrap();
        let refresh = set.refresh(&registry, None).unwrap().clone();
        assert!(refresh.is_empty(), "nothing was adopted");
        assert!(refresh.rejected["example.review"].contains("process.exec:**"));
        set.commit_at_turn_boundary();
        assert_eq!(
            set.loaded()["example.review"],
            manifest.version,
            "the previously loaded version stays"
        );

        // A version bump within the grant is adopted — but only once a turn
        // boundary commits it.
        std::fs::write(
            installed.directory.join(MANIFEST_FILE),
            MANIFEST.replace("1.2.0", "1.3.0"),
        )
        .unwrap();
        let staged = set.refresh(&registry, None).unwrap().clone();
        assert_eq!(staged.updated, vec!["example.review".to_owned()]);
        assert_eq!(
            set.loaded()["example.review"],
            "1.2.0",
            "a staged refresh does not change the running set"
        );
        assert!(set.pending().is_some());
        let committed = set.commit_at_turn_boundary().unwrap();
        assert_eq!(committed.updated, vec!["example.review".to_owned()]);
        assert_eq!(set.loaded()["example.review"], "1.3.0");
        assert_eq!(
            committed.plugins()["example.review"].manifest.version,
            "1.3.0"
        );
        assert!(set.pending().is_none());

        // Discarding a staged refresh leaves the running set alone.
        std::fs::write(
            installed.directory.join(MANIFEST_FILE),
            MANIFEST.replace("1.2.0", "1.4.0"),
        )
        .unwrap();
        set.refresh(&registry, None).unwrap();
        assert!(set.discard().is_some());
        assert_eq!(set.loaded()["example.review"], "1.3.0");

        // Removing the source stages a removal.
        std::fs::remove_dir_all(&installed.directory).unwrap();
        let staged = set.refresh(&registry, None).unwrap().clone();
        assert_eq!(staged.removed, vec!["example.review".to_owned()]);
        set.commit_at_turn_boundary();
        assert!(set.loaded().is_empty());

        // The record survives the process that wrote it.
        registry.record_loaded(set.loaded()).unwrap();
        assert!(registry.loaded_versions().unwrap().is_empty());
    }

    #[test]
    fn one_broken_plugin_does_not_hide_the_others() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path();
        let registry = Registry::open(workspace);
        let good = source(workspace, MANIFEST);
        let (manifest, _) = Registry::inspect_source(&good).unwrap();
        registry
            .install(&good, &Grant::for_manifest(&manifest, 1), 1)
            .unwrap();

        let broken = registry.directory("example.broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join(MANIFEST_FILE), "manifest_version = 9\n").unwrap();

        let (listed, failures) = registry.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "example.broken");

        let mut set = ExtensionSet::new(BTreeMap::new());
        let refresh = set.refresh(&registry, None).unwrap().clone();
        assert_eq!(refresh.added, vec!["example.review".to_owned()]);
        assert!(refresh.rejected.contains_key("example.broken"));
    }
}
