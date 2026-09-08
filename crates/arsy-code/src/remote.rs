//! `remote.exec`: running a command somewhere other than here.
//!
//! Three properties make this safe enough to exist, and they are the reason it
//! is a thin layer over `process.exec` rather than its own execution stack:
//!
//! * **The destination is never an argument.** A request names a target that
//!   configuration already defined, so nothing a model produces can decide
//!   which machine a command reaches. An unknown name is refused.
//! * **Host verification is not negotiable.** The SSH argv is built here and
//!   cannot be extended by a caller, so `StrictHostKeyChecking` cannot be
//!   turned off by anything short of editing this file.
//! * **The remote command is quoted, not concatenated.** SSH runs its argument
//!   through the remote shell, so every element is single-quoted before it is
//!   joined; a filename with a space or a semicolon is an argument, never a
//!   second command.
//!
//! The local client still runs as a subprocess, so the contract declares
//! `process.exec` as well as `remote.exec`: reaching another machine costs the
//! right to start a program on this one, and policy is told so rather than
//! having it hidden inside the executor.

use crate::process::ProcessExecutor;
use arsy_kernel::{
    capability::{CapabilityAction, CapabilityGrant},
    config::RemoteTarget,
    domain::OperationId,
    operation::{
        ConcurrencyRule, Idempotency, InputSchema, JsonType, OperationContract, OperationError,
        OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

/// SSH options this build always sets, in the order it sets them.
///
/// `BatchMode` refuses a password prompt rather than hanging on one no operator
/// is watching. The host key checks are *not* here to be relaxed: an executor
/// that connected to an unverified host would make every later guarantee about
/// where a command ran meaningless.
const SSH_OPTIONS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=yes",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "RequestTTY=no",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteInput {
    /// A configured target name, never a host.
    target: String,
    argv: Vec<String>,
    timeout_ms: u64,
    max_output_bytes: u64,
}

pub struct RemoteExecutor {
    contract: OperationContract,
    targets: BTreeMap<String, RemoteTarget>,
    process: ProcessExecutor,
}

impl RemoteExecutor {
    /// `targets` is the configured set; nothing outside it is reachable.
    pub fn new(
        targets: impl IntoIterator<Item = (String, RemoteTarget)>,
        process: ProcessExecutor,
    ) -> Self {
        Self {
            contract: OperationContract {
                kind: OperationKind::new("remote.exec").expect("static operation kind is valid"),
                input_schema: InputSchema {
                    required: BTreeMap::from([
                        ("target".into(), JsonType::String),
                        ("argv".into(), JsonType::Array),
                        ("timeout_ms".into(), JsonType::Number),
                        ("max_output_bytes".into(), JsonType::Number),
                    ]),
                    optional: BTreeMap::new(),
                    allow_extra: false,
                },
                actions: vec![CapabilityAction::RemoteExec, CapabilityAction::ProcessExec],
                idempotency: Idempotency::Effectful,
                reversible: false,
                concurrency: ConcurrencyRule::Parallel,
            },
            targets: targets.into_iter().collect(),
            process,
        }
    }

    pub fn target_names(&self) -> impl Iterator<Item = &String> {
        self.targets.keys()
    }
}

impl OperationExecutor for RemoteExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        if !request
            .requirements
            .iter()
            .any(|requirement| requirement.action == CapabilityAction::RemoteExec)
        {
            return Err(OperationError::Schema(
                "remote.exec requires a remote.exec capability requirement".into(),
            ));
        }
        let input: RemoteInput = serde_json::from_value(request.input.clone())
            .map_err(|error| OperationError::Schema(error.to_string()))?;
        let target = self.targets.get(&input.target).ok_or_else(|| {
            OperationError::Schema(format!(
                "no remote target named `{}` is configured; known targets: {}",
                input.target,
                self.targets.keys().cloned().collect::<Vec<_>>().join(", ")
            ))
        })?;
        if input.argv.iter().any(String::is_empty) || input.argv.is_empty() {
            return Err(OperationError::Schema(
                "argv must be non-empty and contain no empty element".into(),
            ));
        }

        // The local client is dispatched through the same executor a local
        // command uses, so timeouts, output caps, process-group cleanup, and
        // artifact capture behave identically wherever a command runs.
        let local = OperationRequest {
            id: OperationId::new(),
            kind: OperationKind::new("process.exec").expect("static operation kind is valid"),
            actor: request.actor.clone(),
            requirements: request
                .requirements
                .iter()
                .filter(|requirement| requirement.action == CapabilityAction::ProcessExec)
                .cloned()
                .collect(),
            input: json!({
                "argv": argv(target, &input.argv),
                "timeout_ms": input.timeout_ms,
                "max_output_bytes": input.max_output_bytes,
            }),
        };
        if local.requirements.is_empty() {
            return Err(OperationError::Schema(
                "remote.exec also needs a process.exec requirement: it starts a local client"
                    .into(),
            ));
        }
        self.process.execute(&local, grants)
    }
}

/// The local command line that reaches `target`.
pub fn argv(target: &RemoteTarget, remote: &[String]) -> Vec<String> {
    match target {
        RemoteTarget::Ssh {
            host,
            user,
            port,
            identity,
        } => {
            let mut argv = vec!["ssh".to_owned()];
            argv.extend(SSH_OPTIONS.iter().map(|option| (*option).to_owned()));
            if let Some(identity) = identity {
                argv.push("-i".to_owned());
                argv.push(identity.clone());
                // With an explicit key, agent keys would silently take
                // precedence; naming one means using that one.
                argv.push("-o".to_owned());
                argv.push("IdentitiesOnly=yes".to_owned());
            }
            if let Some(port) = port {
                argv.push("-p".to_owned());
                argv.push(port.to_string());
            }
            argv.push(match user {
                Some(user) => format!("{user}@{host}"),
                None => host.clone(),
            });
            argv.push("--".to_owned());
            // SSH concatenates whatever follows and hands it to the remote
            // shell, so the quoting has to happen here.
            argv.push(
                remote
                    .iter()
                    .map(|word| shell_quote(word))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            argv
        }
        RemoteTarget::Container { engine, container } => {
            // `exec` takes an argv directly: no shell, so no quoting.
            let mut argv = vec![
                engine.clone(),
                "exec".to_owned(),
                "--interactive=false".to_owned(),
                container.clone(),
            ];
            argv.extend(remote.iter().cloned());
            argv
        }
    }
}

/// POSIX single-quoting: everything inside `'` is literal, and an embedded `'`
/// is closed, escaped, and reopened. This is the whole rule, and it is why
/// nothing else needs escaping.
fn shell_quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:=@,+".contains(&byte))
    {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        artifact::{ArtifactStore, FileArtifactStore},
        capability::{CapabilityRequirement, ResourcePattern, ResourceScope},
        domain::{GrantId, Principal, ResourceRef},
    };
    use std::{sync::Arc, time::Duration};

    fn ssh() -> RemoteTarget {
        RemoteTarget::Ssh {
            host: "build.example.test".to_owned(),
            user: Some("agent".to_owned()),
            port: Some(2222),
            identity: Some("/keys/agent".to_owned()),
        }
    }

    #[test]
    fn the_ssh_command_line_verifies_the_host_and_quotes_the_command() {
        let line = argv(&ssh(), &["echo".to_owned(), "a b; rm -rf /".to_owned()]);
        assert_eq!(line[0], "ssh");
        assert!(
            line.windows(2)
                .any(|pair| pair == ["-o", "StrictHostKeyChecking=yes"]),
            "{line:?}"
        );
        assert!(line.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]));
        assert!(
            !line
                .iter()
                .any(|argument| argument.contains("StrictHostKeyChecking=no")),
            "host verification must not be reachable from here"
        );
        assert!(line.windows(2).any(|pair| pair == ["-i", "/keys/agent"]));
        assert!(line.windows(2).any(|pair| pair == ["-p", "2222"]));
        assert_eq!(line[line.len() - 2], "--");
        assert_eq!(
            line[line.len() - 1],
            r"echo 'a b; rm -rf /'",
            "the second word stays one argument"
        );

        // A single quote in an argument cannot end the quoting.
        let line = argv(&ssh(), &["echo".to_owned(), "it's".to_owned()]);
        assert_eq!(line[line.len() - 1], r"echo 'it'\''s'");
    }

    #[test]
    fn a_container_target_passes_argv_through_without_a_shell() {
        let line = argv(
            &RemoteTarget::Container {
                engine: "podman".to_owned(),
                container: "builder".to_owned(),
            },
            &["sh".to_owned(), "-c".to_owned(), "echo 'hi'".to_owned()],
        );
        assert_eq!(
            line,
            vec![
                "podman".to_owned(),
                "exec".to_owned(),
                "--interactive=false".to_owned(),
                "builder".to_owned(),
                "sh".to_owned(),
                "-c".to_owned(),
                "echo 'hi'".to_owned(),
            ],
            "no shell is involved, so nothing is quoted"
        );
    }

    #[test]
    fn quoting_leaves_ordinary_words_alone() {
        for plain in ["echo", "/usr/bin/env", "--flag=value", "a-b_c.d", "x@y:1"] {
            assert_eq!(shell_quote(plain), plain);
        }
        for quoted in ["", "a b", "a;b", "a\nb", "$HOME", "*", "`x`"] {
            assert!(shell_quote(quoted).starts_with('\''), "{quoted}");
        }
    }

    fn executor(targets: &[(&str, RemoteTarget)]) -> (tempfile::TempDir, RemoteExecutor) {
        let directory = tempfile::tempdir().unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(directory.path(), 0).unwrap());
        let process =
            ProcessExecutor::new(artifacts, ["PATH".to_owned()], Duration::from_secs(1), 0);
        (
            directory,
            RemoteExecutor::new(
                targets
                    .iter()
                    .map(|(name, target)| ((*name).to_owned(), target.clone())),
                process,
            ),
        )
    }

    fn request(input: serde_json::Value, actions: &[CapabilityAction]) -> OperationRequest {
        OperationRequest {
            id: OperationId::new(),
            kind: OperationKind::new("remote.exec").unwrap(),
            actor: Principal::User("dev".into()),
            requirements: actions
                .iter()
                .map(|action| CapabilityRequirement {
                    action: *action,
                    resource: ResourceRef::new("remote", "build").unwrap(),
                })
                .collect(),
            input,
        }
    }

    fn grant(action: CapabilityAction) -> CapabilityGrant {
        CapabilityGrant {
            id: GrantId::new(),
            actor: Principal::User("dev".into()),
            action,
            scope: ResourceScope::single(ResourcePattern::new("remote", "**").unwrap()),
            expires_at_ms: None,
            delegation_depth: 0,
            source: arsy_kernel::capability::PolicySource::User,
        }
    }

    #[test]
    fn an_unconfigured_target_is_refused_and_names_the_ones_that_exist() {
        let (_directory, executor) = executor(&[("build", ssh())]);
        let grants = [
            grant(CapabilityAction::RemoteExec),
            grant(CapabilityAction::ProcessExec),
        ];
        let error = executor
            .execute(
                &request(
                    json!({
                        "target": "production",
                        "argv": ["echo", "hi"],
                        "timeout_ms": 1_000,
                        "max_output_bytes": 1_024,
                    }),
                    &[CapabilityAction::RemoteExec, CapabilityAction::ProcessExec],
                ),
                &grants,
            )
            .expect_err("an unknown target is not reachable");
        assert!(
            matches!(&error, OperationError::Schema(message)
                if message.contains("production") && message.contains("build")),
            "{error:?}"
        );
        assert_eq!(executor.target_names().collect::<Vec<_>>(), ["build"]);
    }

    #[test]
    fn reaching_another_machine_needs_the_right_to_start_one_here() {
        let (_directory, executor) = executor(&[("build", ssh())]);
        let input = json!({
            "target": "build",
            "argv": ["echo", "hi"],
            "timeout_ms": 1_000,
            "max_output_bytes": 1_024,
        });

        // The contract says so up front, so policy sees both actions.
        assert_eq!(
            executor.contract().actions,
            vec![CapabilityAction::RemoteExec, CapabilityAction::ProcessExec]
        );

        // A request carrying only `remote.exec` is refused rather than quietly
        // starting a local process on a grant that never covered one.
        let error = executor
            .execute(
                &request(input.clone(), &[CapabilityAction::RemoteExec]),
                &[grant(CapabilityAction::RemoteExec)],
            )
            .expect_err("a local client still has to be permitted");
        assert!(
            matches!(&error, OperationError::Schema(message)
                if message.contains("process.exec")),
            "{error:?}"
        );

        // And one carrying neither is refused for the action it is named after.
        let error = executor
            .execute(
                &request(input, &[CapabilityAction::ProcessExec]),
                &[grant(CapabilityAction::ProcessExec)],
            )
            .expect_err("remote.exec needs its own requirement");
        assert!(
            matches!(&error, OperationError::Schema(message)
                if message.contains("remote.exec requires")),
            "{error:?}"
        );
    }

    #[test]
    fn an_empty_argument_is_refused_before_anything_is_spawned() {
        let (_directory, executor) = executor(&[("build", ssh())]);
        for argv in [json!([]), json!(["echo", ""])] {
            let error = executor
                .execute(
                    &request(
                        json!({
                            "target": "build",
                            "argv": argv,
                            "timeout_ms": 1_000,
                            "max_output_bytes": 1_024,
                        }),
                        &[CapabilityAction::RemoteExec, CapabilityAction::ProcessExec],
                    ),
                    &[
                        grant(CapabilityAction::RemoteExec),
                        grant(CapabilityAction::ProcessExec),
                    ],
                )
                .expect_err("an empty argument is a mistake, not a command");
            assert!(matches!(error, OperationError::Schema(_)));
        }
    }
}
