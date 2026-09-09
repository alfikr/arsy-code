//! Running an installed WASM plugin, as an operation like any other.
//!
//! # Two gates, and why both
//!
//! Policy decides whether *this* caller may invoke *that* plugin — a
//! `plugin.invoke` requirement over the plugin's id, so a rule can allow one
//! plugin and not another. The WASM host then decides what the plugin may do
//! once it runs: its manifest's capabilities must be inside the grant the
//! operator recorded at install, and the module may import only the host
//! functions the manifest declared.
//!
//! Neither gate covers the other. The first is about who is asking; the second
//! is about what was approved, months ago, by someone who read the manifest.
//!
//! # What a plugin can reach
//!
//! One byte in, one byte out, through two host functions. That is not a
//! limitation to be lifted later: a plugin that could call the workspace
//! directly would be a second tool runtime with its own policy story. When a
//! plugin needs to read a file, it will ask the host to run `fs.read` and get
//! the answer back through the same two functions.

use crate::{
    extension::{ExtensionHost, InvocationLimits, PluginManifest},
    plugin::Registry,
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
use serde::Serialize;
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

/// What one invocation may spend.
///
/// Deliberately not configurable yet: these bound a plugin that misbehaves,
/// and the number that matters is the one an operator did not have to choose
/// correctly. A plugin that legitimately needs more is a reason to add a
/// manifest field, not a reason to raise the default for everyone.
pub const LIMITS: InvocationLimits = InvocationLimits {
    fuel: 200_000_000,
    timeout: Duration::from_secs(5),
    memory_bytes: 16 * 1024 * 1024,
    output_bytes: 1024 * 1024,
};

#[derive(Debug, Serialize)]
struct PluginOutcome {
    plugin: String,
    version: String,
    /// The plugin's output as text when it is text, which is what a caller
    /// almost always wants and what the model can read.
    output: String,
    output_bytes: usize,
}

pub struct PluginExecutor {
    contract: OperationContract,
    workspace: PathBuf,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
}

impl PluginExecutor {
    pub fn new(
        workspace: &Workspace,
        artifacts: Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            contract: OperationContract {
                kind: OperationKind::new("plugin.invoke").expect("static operation kind is valid"),
                input_schema: InputSchema {
                    required: BTreeMap::from([("plugin".to_owned(), JsonType::String)]),
                    optional: BTreeMap::from([("input".to_owned(), JsonType::String)]),
                    allow_extra: false,
                },
                actions: vec![CapabilityAction::PluginInvoke],
                // A plugin is arbitrary code: nothing here can promise that
                // running it twice does what running it once did.
                idempotency: Idempotency::Effectful,
                // Reversible because this host gives a plugin no way to change
                // anything: bytes in, bytes out, and the result is an artifact.
                // The first host call that reaches the workspace makes this
                // false, and the policy engine will then require an approval
                // for exactly the reason it should.
                reversible: true,
                concurrency: ConcurrencyRule::ExclusivePerResource,
            },
            workspace: workspace.path().to_owned(),
            artifacts,
            retain_until_ms,
        })
    }
}

impl OperationExecutor for PluginExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let id = request
            .input
            .get("plugin")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let input = request
            .input
            .get("input")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let registry = Registry::open(&self.workspace);
        let installed = registry
            .get(id)
            .map_err(|error| OperationError::Execution(error.to_string()))?;
        if !installed.loadable() {
            return Err(OperationError::Execution(format!(
                "{id} asks for {} beyond what was approved; re-install it to approve the change",
                installed.grant.excess(&installed.manifest).join(", ")
            )));
        }
        let module = std::fs::read(installed.directory.join(&installed.manifest.entrypoint))
            .map_err(|error| {
                OperationError::Execution(format!(
                    "{id}: {} is unreadable: {error}",
                    installed.manifest.entrypoint
                ))
            })?;

        let actions = installed.manifest.actions();
        let mut host = ExtensionHost::new().map_err(execution)?;
        host.install(
            PluginManifest {
                id: installed.manifest.id.clone(),
                version: installed.manifest.version.clone(),
                imports: installed.manifest.imports.clone(),
                capabilities: actions.clone(),
            },
            &module,
            // What the operator approved at install time is the ceiling. The
            // manifest is checked against it rather than trusted.
            actions.clone(),
        )
        .map_err(execution)?;
        let output = host
            .invoke(id, input.as_bytes(), &actions, LIMITS)
            .map_err(execution)?;

        let outcome = PluginOutcome {
            plugin: installed.manifest.id,
            version: installed.manifest.version,
            output_bytes: output.len(),
            output: String::from_utf8_lossy(&output).into_owned(),
        };
        let value = super::store(
            self.artifacts.as_ref(),
            &outcome,
            request.actor.clone(),
            self.retain_until_ms,
        )?;
        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: CapabilityAction::PluginInvoke,
                resource: ResourceRef::new("plugin", id)
                    .unwrap_or_else(|_| ResourceRef::new("plugin", "*").expect("a static value")),
            }],
            evidence: Vec::new(),
            state: None,
        })
    }
}

fn execution(error: crate::extension::ExtensionError) -> OperationError {
    OperationError::Execution(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{Grant, Manifest};
    use arsy_kernel::{
        artifact::{ArtifactReadLimits, FileArtifactStore},
        domain::{OperationId, Principal},
    };

    /// A module that reads the input a byte at a time and emits it upper-cased,
    /// which is enough to prove the host's two functions both work.
    const SHOUT: &str = r#"
        (module
          (import "arsy" "read_input_byte" (func $read (param i32) (result i32)))
          (import "arsy" "emit_byte" (func $emit (param i32) (result i32)))
          (func (export "run")
            (local $index i32) (local $byte i32)
            (block $done
              (loop $next
                (local.set $byte (call $read (local.get $index)))
                (br_if $done (i32.lt_s (local.get $byte) (i32.const 0)))
                (if (i32.and
                      (i32.ge_u (local.get $byte) (i32.const 97))
                      (i32.le_u (local.get $byte) (i32.const 122)))
                  (then (local.set $byte (i32.sub (local.get $byte) (i32.const 32)))))
                (drop (call $emit (local.get $byte)))
                (local.set $index (i32.add (local.get $index) (i32.const 1)))
                (br $next))))
        )
    "#;

    fn install(root: &std::path::Path, id: &str, module: &str, imports: &str) {
        let directory = root.join(crate::plugin::PLUGIN_DIRECTORY).join(id);
        std::fs::create_dir_all(&directory).unwrap();
        let manifest = format!(
            "manifest_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\n\
             entrypoint = \"plugin.wat\"\napi = \"1\"\n\
             capabilities = [\"plugin.invoke:plugin/**\"]\nimports = [{imports}]\n"
        );
        std::fs::write(directory.join(crate::plugin::MANIFEST_FILE), &manifest).unwrap();
        std::fs::write(directory.join("plugin.wat"), module).unwrap();
        let grant = Grant::for_manifest(&Manifest::parse(&manifest).unwrap(), 0);
        std::fs::write(
            directory.join(crate::plugin::GRANT_FILE),
            serde_json::to_string(&grant).unwrap(),
        )
        .unwrap();
    }

    fn invoke(root: &std::path::Path, plugin: &str, input: &str) -> Result<Value, OperationError> {
        let workspace = Workspace::open(root).unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(root.join(".arsy/art"), 0).unwrap());
        let executor = PluginExecutor::new(&workspace, Arc::clone(&artifacts), 0);
        let outcome = executor.execute(
            &OperationRequest {
                id: OperationId::new(),
                kind: OperationKind::new("plugin.invoke").unwrap(),
                actor: Principal::System,
                input: serde_json::json!({"plugin": plugin, "input": input}),
                requirements: Vec::new(),
            },
            &[],
        )?;
        let reference = outcome.value.expect("an invocation stores its output");
        let id: arsy_kernel::domain::ArtifactId = reference.value().parse().unwrap();
        let bytes = artifacts
            .read(
                id,
                ArtifactReadLimits {
                    max_bytes: 1024 * 1024,
                    max_expansion_ratio: 1_000,
                },
            )
            .unwrap();
        Ok(serde_json::from_slice(&bytes).unwrap())
    }

    #[test]
    fn an_installed_plugin_runs_and_its_output_is_evidence() {
        let root = tempfile::tempdir().unwrap();
        install(
            root.path(),
            "shout",
            SHOUT,
            "\"arsy::read_input_byte\", \"arsy::emit_byte\"",
        );

        let outcome = invoke(root.path(), "shout", "hello").unwrap();

        assert_eq!(outcome["plugin"], "shout");
        assert_eq!(outcome["output"], "HELLO");
        assert_eq!(outcome["output_bytes"], 5);
    }

    #[test]
    fn a_module_that_imports_what_its_manifest_never_declared_does_not_run() {
        let root = tempfile::tempdir().unwrap();
        // The module still imports both functions; the manifest admits one.
        install(root.path(), "sneak", SHOUT, "\"arsy::read_input_byte\"");

        let error = invoke(root.path(), "sneak", "hello").unwrap_err();

        assert!(format!("{error}").contains("emit_byte"), "{error}");
    }

    #[test]
    fn a_plugin_this_workspace_never_installed_is_refused_by_name() {
        let root = tempfile::tempdir().unwrap();
        let error = invoke(root.path(), "absent", "").unwrap_err();
        assert!(format!("{error}").contains("absent"), "{error}");
    }
}
