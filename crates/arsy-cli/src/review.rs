//! `arsy review`: what the working tree changed, and what that deserves.
//!
//! The change comes from Git rather than from the session, so a review is
//! about the repository as it stands — including whatever an operator edited
//! by hand next to the agent's work. Nothing here decides; it reports the
//! signals and the depth they imply, and `--strict` turns that report into an
//! exit code a pipeline can gate on.

use crate::{usage, Command, Diagnostic, Emitter, Invocation, Output};
use arsy_code::review::{assess, parse_diff, FindingKind, Review};
use serde_json::{json, Value};
use std::{
    io::Read,
    path::Path,
    process::{Command as Process, Stdio},
};

/// Enough diff to read a large refactor, little enough that a generated file
/// cannot turn a review into a memory problem.
const MAX_DIFF_BYTES: usize = 8 * 1024 * 1024;

pub fn parse(arguments: &crate::ParsedArguments) -> Result<Command, Diagnostic> {
    let mut positional = arguments.positional.clone();
    if positional.len() > 1 {
        return Err(usage("review takes at most one revision"));
    }
    // The positional and the flag name the same thing, so saying both is a
    // question about which one was meant rather than an answer.
    if !positional.is_empty() && arguments.base.is_some() {
        return Err(usage(
            "review takes a revision either as an argument or as --base, not both",
        ));
    }
    Ok(Command::Review {
        base: positional
            .pop()
            .or_else(|| arguments.base.clone())
            .unwrap_or_else(|| "HEAD".to_owned()),
        strict: arguments.strict,
    })
}

pub fn run(
    invocation: &Invocation,
    base: &str,
    strict: bool,
    emitter: &mut Emitter,
) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let review = assess(parse_diff(&diff(&root, base)?), None);

    let blocking = strict && !review.findings.is_empty();
    emitter.result(if emitter.output == Output::Json {
        let mut report = serde_json::to_value(&review).map_err(crate::storage_failed)?;
        crate::merge(&mut report, json!({"base": base}));
        report
    } else {
        json!({"review": human(&review)})
    });
    // Exit 7 is the verification class: the change was read, and it is not
    // ready by the standard the caller asked for.
    Ok(i32::from(blocking) * 7)
}

/// Everything the working tree changed since `base`, staged or not.
///
/// `git diff <base>` rather than `git diff`, because a review that ignored the
/// index would miss exactly the changes someone was about to commit. `HEAD` is
/// the default, and any revision a branch review needs -- `main`,
/// `origin/main`, a tag -- is the same command with a different name.
fn diff(root: &Path, base: &str) -> Result<String, Diagnostic> {
    let mut child = Process::new("git")
        .args(["--no-pager", "diff", base, "--no-color", "--no-ext-diff"])
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            Diagnostic::error(
                "ARSY-VER-1000",
                format!("git could not be run: {error}"),
                "install Git, or review a workspace that is a Git repository",
            )
        })?;
    let mut output = String::new();
    child
        .stdout
        .take()
        .expect("stdout is piped")
        .take(MAX_DIFF_BYTES as u64)
        .read_to_string(&mut output)
        .map_err(|error| {
            Diagnostic::error(
                "ARSY-VER-1000",
                format!("the diff is not UTF-8 text: {error}"),
                "review a workspace whose changed files are text",
            )
        })?;
    let status = child.wait().map_err(crate::storage_failed)?;
    if !status.success() {
        return Err(Diagnostic::error(
            "ARSY-VER-1000",
            format!("git could not describe what changed since `{base}`"),
            "name a revision this repository has, and run `arsy review` inside a Git repository \
             with at least one commit",
        ));
    }
    Ok(output)
}

fn human(review: &Review) -> String {
    if review.files.is_empty() {
        return "Nothing has changed since the last commit.".to_owned();
    }
    let mut text = format!(
        "{} file(s), +{} -{} · verification: {}\n",
        review.files.len(),
        review.files.iter().map(|file| file.added).sum::<u32>(),
        review.files.iter().map(|file| file.removed).sum::<u32>(),
        depth(review),
    );
    for finding in &review.findings {
        text.push_str(&format!("  {}: {}\n", kind(finding.kind), finding.message));
    }
    if review.findings.is_empty() {
        text.push_str("  nothing to look at beyond the change itself\n");
    }
    text
}

fn depth(review: &Review) -> String {
    serde_json::to_value(review.depth)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn kind(kind: FindingKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value: Value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "finding".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_takes_one_revision_either_way_and_defaults_to_head() {
        let command = |args: &[&str]| {
            crate::parse(args.iter().map(|argument| (*argument).to_owned())).map(|it| it.command)
        };
        assert_eq!(
            command(&["review"]).unwrap(),
            Command::Review {
                base: "HEAD".to_owned(),
                strict: false
            }
        );
        assert_eq!(
            command(&["review", "main", "--strict"]).unwrap(),
            Command::Review {
                base: "main".to_owned(),
                strict: true
            }
        );
        assert_eq!(
            command(&["review", "--base", "origin/main"]).unwrap(),
            Command::Review {
                base: "origin/main".to_owned(),
                strict: false
            }
        );
        // Two revisions, or the same revision twice over, is a question.
        assert!(command(&["review", "main", "--base", "dev"]).is_err());
        assert!(command(&["review", "main", "dev"]).is_err());
    }
}
