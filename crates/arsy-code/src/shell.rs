//! Conservative, advisory effect prediction for POSIX-like shell input.

use arsy_kernel::{
    capability::{CapabilityAction, CapabilityRequirement},
    domain::ResourceRef,
    operation::Effect,
};
use serde::Serialize;

const ALL_ACTIONS: [CapabilityAction; 15] = [
    CapabilityAction::FsRead,
    CapabilityAction::FsWrite,
    CapabilityAction::FsDelete,
    CapabilityAction::ProcessExec,
    CapabilityAction::ProcessSignal,
    CapabilityAction::NetworkConnect,
    CapabilityAction::GitRead,
    CapabilityAction::GitWrite,
    CapabilityAction::CredentialUse,
    CapabilityAction::BrowserControl,
    CapabilityAction::DebugLaunch,
    CapabilityAction::DebugAttach,
    CapabilityAction::RemoteExec,
    CapabilityAction::SystemModify,
    CapabilityAction::PluginInvoke,
];

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct AdvisoryPrediction {
    /// Always true: static shell analysis never authorizes execution.
    pub advisory: bool,
    pub requirements: Vec<CapabilityRequirement>,
    pub used_widest_fallback: bool,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct PredictionReconciliation {
    pub advisory: bool,
    pub unexpected_observed: Vec<Effect>,
    pub predicted_but_unobserved: Vec<CapabilityRequirement>,
}

/// Predict common command, pipeline, conditional, and redirection effects.
/// Dynamic expansion, substitution, or malformed syntax fails closed to every
/// capability over an unresolved resource.
pub fn predict(command: &str) -> AdvisoryPrediction {
    let Some(tokens) = tokenize(command) else {
        return widest();
    };
    if tokens.is_empty()
        || tokens
            .iter()
            .any(|token| token.contains(['$', '`', '{', '}', '(', ')']))
    {
        return widest();
    }

    let mut requirements = Vec::new();
    let mut command_start = true;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        match token.as_str() {
            ";" | "&&" | "||" | "|" => command_start = true,
            ">" | ">>" | "<" => {
                let Some(path) = tokens.get(index + 1) else {
                    return widest();
                };
                let action = if token == "<" {
                    CapabilityAction::FsRead
                } else {
                    CapabilityAction::FsWrite
                };
                push(&mut requirements, action, "file", path);
                index += 1;
            }
            _ if command_start => {
                if !predict_program(token, &tokens[index + 1..], &mut requirements) {
                    return widest();
                }
                command_start = false;
            }
            _ => {}
        }
        index += 1;
    }
    AdvisoryPrediction {
        advisory: true,
        requirements,
        used_widest_fallback: false,
    }
}

pub fn reconcile(prediction: &AdvisoryPrediction, observed: &[Effect]) -> PredictionReconciliation {
    PredictionReconciliation {
        advisory: true,
        unexpected_observed: observed
            .iter()
            .filter(|effect| {
                !prediction.requirements.iter().any(|requirement| {
                    requirement.action == effect.action && requirement.resource == effect.resource
                })
            })
            .cloned()
            .collect(),
        predicted_but_unobserved: prediction
            .requirements
            .iter()
            .filter(|requirement| {
                !observed.iter().any(|effect| {
                    requirement.action == effect.action && requirement.resource == effect.resource
                })
            })
            .cloned()
            .collect(),
    }
}

fn predict_program(
    program: &str,
    args: &[String],
    requirements: &mut Vec<CapabilityRequirement>,
) -> bool {
    push(
        requirements,
        CapabilityAction::ProcessExec,
        "process",
        program,
    );
    let program = program.rsplit('/').next().unwrap_or(program);
    if predict_file_program(program, args, requirements) {
        return true;
    }
    match program {
        "curl" | "wget" => {
            push(
                requirements,
                CapabilityAction::NetworkConnect,
                "network",
                "*",
            );
            push(requirements, CapabilityAction::FsWrite, "file", "*");
        }
        "ssh" => {
            push(
                requirements,
                CapabilityAction::NetworkConnect,
                "network",
                "*",
            );
            push(requirements, CapabilityAction::RemoteExec, "remote", "*");
        }
        "git" => {
            let action = match args.first().map(String::as_str) {
                Some("status" | "diff" | "log" | "show" | "branch") => CapabilityAction::GitRead,
                _ => CapabilityAction::GitWrite,
            };
            push(requirements, action, "git", "*");
        }
        "echo" | "printf" | "true" | "false" => {}
        _ => return false,
    }
    true
}

fn predict_file_program(
    program: &str,
    args: &[String],
    requirements: &mut Vec<CapabilityRequirement>,
) -> bool {
    match program {
        "cat" | "head" | "tail" | "less" => {
            for path in args
                .iter()
                .take_while(|arg| !matches!(arg.as_str(), ";" | "&&" | "||" | "|"))
            {
                if !path.starts_with('-') {
                    push(requirements, CapabilityAction::FsRead, "file", path);
                }
            }
        }
        "rm" => {
            for path in args.iter().filter(|arg| !arg.starts_with('-')) {
                push(requirements, CapabilityAction::FsDelete, "file", path);
            }
        }
        "cp" | "mv" => {
            for path in args.iter().filter(|arg| !arg.starts_with('-')) {
                push(requirements, CapabilityAction::FsRead, "file", path);
                push(requirements, CapabilityAction::FsWrite, "file", path);
            }
        }
        "touch" | "mkdir" => {
            for path in args.iter().filter(|arg| !arg.starts_with('-')) {
                push(requirements, CapabilityAction::FsWrite, "file", path);
            }
        }
        _ => return false,
    }
    true
}

fn push(
    requirements: &mut Vec<CapabilityRequirement>,
    action: CapabilityAction,
    scheme: &str,
    value: &str,
) {
    let requirement = CapabilityRequirement::new(
        action,
        ResourceRef::new(scheme, value).expect("static scheme and non-empty token"),
    );
    if !requirements.contains(&requirement) {
        requirements.push(requirement);
    }
}

fn widest() -> AdvisoryPrediction {
    AdvisoryPrediction {
        advisory: true,
        requirements: ALL_ACTIONS
            .into_iter()
            .map(|action| {
                CapabilityRequirement::new(action, ResourceRef::new("unresolved", "*").unwrap())
            })
            .collect(),
        used_widest_fallback: true,
    }
}

fn tokenize(input: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if let Some(end) = quote {
            if ch == end {
                quote = None;
            } else if ch == '\\' && end == '"' {
                current.push(chars.next()?);
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '\\' => current.push(chars.next()?),
            ' ' | '\t' | '\n' => finish(&mut tokens, &mut current),
            ';' | '|' | '&' | '>' | '<' => {
                finish(&mut tokens, &mut current);
                let mut operator = ch.to_string();
                if chars.peek() == Some(&ch) {
                    operator.push(chars.next().unwrap());
                }
                if matches!(operator.as_str(), "&" | "|||") {
                    return None;
                }
                tokens.push(operator);
            }
            _ => current.push(ch),
        }
    }
    if quote.is_some() {
        return None;
    }
    finish(&mut tokens, &mut current);
    Some(tokens)
}

fn finish(tokens: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        tokens.push(std::mem::take(current));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decomposes_pipeline_conditional_and_redirections_as_advisory() {
        let prediction = predict("cat input | curl example.test > output && rm old");
        assert!(prediction.advisory);
        assert!(!prediction.used_widest_fallback);
        for action in [
            CapabilityAction::FsRead,
            CapabilityAction::NetworkConnect,
            CapabilityAction::FsWrite,
            CapabilityAction::FsDelete,
        ] {
            assert!(prediction
                .requirements
                .iter()
                .any(|requirement| requirement.action == action));
        }
    }

    #[test]
    fn dynamic_or_unparseable_input_uses_widest_requirement() {
        for command in ["echo $UNKNOWN", "echo 'unterminated", "unknown-command"] {
            let prediction = predict(command);
            assert!(prediction.used_widest_fallback);
            assert_eq!(prediction.requirements.len(), ALL_ACTIONS.len());
        }
    }

    #[test]
    fn reconciliation_records_both_directions_of_divergence() {
        let prediction = predict("cat expected");
        let observed = vec![Effect {
            action: CapabilityAction::FsRead,
            resource: ResourceRef::new("file", "other").unwrap(),
        }];
        let reconciliation = reconcile(&prediction, &observed);
        assert!(reconciliation.advisory);
        assert_eq!(reconciliation.unexpected_observed, observed);
        assert!(!reconciliation.predicted_but_unobserved.is_empty());
    }
}
