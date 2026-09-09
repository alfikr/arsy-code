//! The registry of operations this build can dispatch.
//!
//! One place, so the contracts a dry run explains are the contracts an
//! execution would dispatch against. A kind that is not here cannot run, and
//! `arsy policy explain` says so rather than inventing a requirement.

use crate::{
    git::{GitExecutor, GitOperation},
    process::ProcessExecutor,
    remote::RemoteExecutor,
    resource::Workspace,
};
use arsy_kernel::{
    artifact::ArtifactStore,
    capability::{CapabilityAction, CapabilityRequirement},
    domain::ResourceRef,
    operation::{OperationContract, OperationRegistry, RegistrationError},
};
use serde_json::Value;
use std::{path::Path, sync::Arc, time::Duration};

/// The concrete resources one call needs authority over.
///
/// A contract names the actions an operation may need; the resources arrive
/// with the request. This is the one place that mapping lives, so a dry run and
/// a dispatch can never disagree about what a call is asking for.
pub fn requirements(
    contract: &OperationContract,
    input: &Value,
    workspace: &Path,
) -> Vec<CapabilityRequirement> {
    contract
        .actions
        .iter()
        .map(|action| CapabilityRequirement {
            action: *action,
            resource: resource_for(*action, input, workspace),
        })
        .collect()
}

/// What an action is exercised over, read from the call's own input where the
/// input says, and from the workspace where it does not.
fn resource_for(action: CapabilityAction, input: &Value, workspace: &Path) -> ResourceRef {
    let string = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
    // The scheme is the action's own, so a rule written against it matches
    // wherever the resource is built.
    let scheme = action.default_scheme();
    let value = match action {
        CapabilityAction::FsRead
        | CapabilityAction::FsWrite
        | CapabilityAction::FsDelete
        | CapabilityAction::GitRead
        | CapabilityAction::GitWrite => {
            string("path").unwrap_or_else(|| workspace.display().to_string())
        }
        CapabilityAction::ProcessExec | CapabilityAction::ProcessSignal => input
            .get("argv")
            .and_then(Value::as_array)
            .and_then(|argv| argv.first())
            .and_then(Value::as_str)
            .unwrap_or("*")
            .to_owned(),
        CapabilityAction::RemoteExec => string("target").unwrap_or_else(|| "*".into()),
        CapabilityAction::NetworkConnect => string("host").unwrap_or_else(|| "*".into()),
        CapabilityAction::CredentialUse => string("handle").unwrap_or_else(|| "*".into()),
        CapabilityAction::PluginInvoke => string("plugin").unwrap_or_else(|| "*".into()),
        CapabilityAction::BrowserControl
        | CapabilityAction::DebugLaunch
        | CapabilityAction::DebugAttach
        | CapabilityAction::SystemModify => "*".to_owned(),
    };
    ResourceRef::new(scheme, value)
        .unwrap_or_else(|_| ResourceRef::new(scheme, "*").expect("a static scheme and value"))
}

/// Environment variables a subprocess inherits. Everything else is dropped, so
/// a credential in the operator's shell cannot reach a tool by accident.
pub const DEFAULT_ENVIRONMENT_ALLOWLIST: &[&str] = &["PATH", "HOME", "LANG", "TZ"];

/// How long a terminated process has to exit before it is killed.
pub const DEFAULT_TERMINATION_GRACE: Duration = Duration::from_secs(5);

/// What a workspace may reach beyond its own files, as configuration resolved
/// it.
///
/// One parameter rather than two lists, because they are the same decision
/// made twice: which programs and machines this workspace is allowed to
/// involve. A registry built with the default reaches nothing outside itself.
#[derive(Clone, Debug, Default)]
pub struct Reachable {
    pub remote_targets: Vec<(String, arsy_kernel::config::RemoteTarget)>,
    pub language_servers: Vec<arsy_kernel::config::LanguageServer>,
}

impl Reachable {
    /// Everything configuration says this workspace may reach.
    pub fn from_config(config: &arsy_kernel::config::Config) -> Self {
        Self {
            remote_targets: config
                .remote_targets()
                .map(|(name, target)| (name.clone(), target.clone()))
                .collect(),
            language_servers: config.language_servers().cloned().collect(),
        }
    }
}

/// Build the registry for one workspace.
///
/// `retain_until_ms` is stamped on the artifacts operations produce, so `gc`
/// knows when their evidence may be collected.
pub fn registry(
    workspace: &Workspace,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
    reachable: Reachable,
) -> Result<OperationRegistry, RegistrationError> {
    let mut registry = OperationRegistry::new();
    for operation in [
        GitOperation::Status,
        GitOperation::Diff,
        GitOperation::Log,
        GitOperation::Blame,
    ] {
        registry.register(GitExecutor::new(
            operation,
            workspace,
            Arc::clone(&artifacts),
            retain_until_ms,
        ))?;
    }
    // The workspace operations. They are registered here rather than by the
    // agent so that `arsy policy explain`, the MCP server, and a turn all see
    // the same set: a tool the model can call is a tool an operator can reason
    // about beforehand.
    for executor in crate::agent::fsops::executors(workspace, &artifacts, retain_until_ms)
        .into_iter()
        .chain(crate::agent::searchops::executors(
            workspace,
            &artifacts,
            retain_until_ms,
        ))
        .chain(crate::agent::codeops::executors(
            workspace,
            &artifacts,
            retain_until_ms,
            reachable.language_servers,
        ))
    {
        registry.register(executor)?;
    }
    #[cfg(feature = "dap")]
    registry.register(crate::agent::debugops::DebugExecutor::new(
        workspace,
        Arc::clone(&artifacts),
        retain_until_ms,
    ))?;
    #[cfg(feature = "wasm")]
    registry.register(crate::agent::pluginops::PluginExecutor::new(
        workspace,
        Arc::clone(&artifacts),
        retain_until_ms,
    ))?;
    registry.register(crate::agent::patch::PatchExecutor::new(
        workspace,
        Arc::clone(&artifacts),
        retain_until_ms,
    ))?;
    let process = |artifacts| {
        ProcessExecutor::new(
            artifacts,
            DEFAULT_ENVIRONMENT_ALLOWLIST
                .iter()
                .map(|name| (*name).to_owned()),
            DEFAULT_TERMINATION_GRACE,
            retain_until_ms,
        )
        // A shell command runs where the same turn's file tools read and write.
        .in_directory(workspace.path())
    };
    // A remote target is only reachable when configuration defined one, so a
    // workspace with none cannot dispatch `remote.exec` at all rather than
    // dispatching it to nowhere.
    if !reachable.remote_targets.is_empty() {
        registry.register(Arc::new(RemoteExecutor::new(
            reachable.remote_targets,
            process(Arc::clone(&artifacts)),
        )))?;
    }
    registry.register(Arc::new(process(artifacts)))?;
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{artifact::FileArtifactStore, operation::OperationKind};

    #[test]
    fn every_registered_kind_publishes_the_actions_it_needs() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(temporary.path()).unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(temporary.path().join("artifacts"), 0).unwrap());
        let registry =
            registry(&workspace, Arc::clone(&artifacts), 0, Reachable::default()).unwrap();

        let kinds: Vec<_> = registry.kinds().map(ToString::to_string).collect();
        // A WASM build can dispatch a plugin; a build without the feature has
        // no host to run one in and does not offer the operation at all.
        #[cfg(feature = "wasm")]
        assert!(kinds.contains(&"plugin.invoke".to_owned()));
        #[cfg(feature = "dap")]
        assert!(kinds.contains(&"debug.run".to_owned()));
        let kinds: Vec<_> = kinds
            .into_iter()
            .filter(|kind| kind != "plugin.invoke" && kind != "debug.run")
            .collect();
        assert_eq!(
            kinds,
            vec![
                "code.diagnostics".to_owned(),
                "code.explain".to_owned(),
                "code.references".to_owned(),
                "code.rename".to_owned(),
                "code.symbol".to_owned(),
                "fs.create".to_owned(),
                "fs.delete".to_owned(),
                "fs.edit".to_owned(),
                "fs.list".to_owned(),
                "fs.move".to_owned(),
                "fs.patch".to_owned(),
                "fs.read".to_owned(),
                "fs.write".to_owned(),
                "git.blame".to_owned(),
                "git.diff".to_owned(),
                "git.log".to_owned(),
                "git.status".to_owned(),
                "process.exec".to_owned(),
                "search.files".to_owned(),
                "search.text".to_owned(),
            ],
            "a workspace with no remote target cannot dispatch one"
        );

        let with_remote = super::registry(
            &workspace,
            artifacts,
            0,
            Reachable {
                remote_targets: vec![(
                    "build".to_owned(),
                    arsy_kernel::config::RemoteTarget::Container {
                        engine: "docker".to_owned(),
                        container: "builder".to_owned(),
                    },
                )],
                ..Reachable::default()
            },
        )
        .unwrap();
        assert!(with_remote
            .kinds()
            .any(|kind| kind.as_str() == "remote.exec"));
        for kind in &kinds {
            let contract = registry
                .contract(&OperationKind::new(kind.clone()).unwrap())
                .expect("a listed kind has a contract");
            assert!(!contract.actions.is_empty(), "{kind} declares no action");
        }
    }
}
