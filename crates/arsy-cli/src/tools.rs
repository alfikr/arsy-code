//! The tools ARSY runs on the model's behalf.
//!
//! Two are offered: `bash`, which runs a shell command in the workspace, and
//! `apply_patch`, which edits files with the patch dialect Codex established —
//! `*** Begin Patch` sections with `@@` context and `+`/`-`/space lines. The
//! dialect is worth copying rather than inventing: models are trained on it,
//! and it carries context, so a hunk that no longer matches is rejected
//! instead of applied to the wrong place.
//!
//! Nothing here decides whether a call may run. The caller confirms every call
//! with the operator first, so this module is only the doing.

use arsy_kernel::provider::ToolSchema;
use serde_json::{json, Value};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Enough output to read a test failure, little enough to leave a turn's
/// context for the answer. The tail is what a command was going to say.
const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// How long a command that ignored its termination signal is given before it
/// is killed, matching the wait the provider child gets.
const KILL_GRACE: Duration = Duration::from_millis(500);

/// What the model is told it can call.
pub fn schemas() -> Vec<ToolSchema> {
    vec![
        ToolSchema {
            name: "bash".to_owned(),
            description: "Run a shell command in the workspace and return its merged stdout and stderr. Output is truncated to the last 16 KiB.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to run."},
                    "timeout_ms": {
                        "type": "number",
                        "description": "Deadline in milliseconds. Defaults to 120000; capped at 600000."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolSchema {
            name: "apply_patch".to_owned(),
            description: concat!(
                "Edit files with a patch. The patch is a string of the form:\n",
                "*** Begin Patch\n",
                "*** Update File: path/to/file.rs\n",
                "@@ optional context line\n",
                " unchanged line\n",
                "-removed line\n",
                "+added line\n",
                "*** End Patch\n",
                "`*** Add File: path` is followed by `+` lines only, ",
                "`*** Delete File: path` takes no body, and `*** Move to: path` ",
                "renames the file being updated. Paths are relative to the workspace."
            )
            .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "patch": {"type": "string", "description": "The patch text, including the Begin/End markers."}
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
        },
    ]
}

/// A one-line summary of a call, for the row shown before it is confirmed.
pub fn summarize(name: &str, arguments: &Value) -> String {
    match name {
        "bash" => arguments["command"]
            .as_str()
            .unwrap_or("<no command>")
            .to_owned(),
        "apply_patch" => {
            let patch = arguments["patch"].as_str().unwrap_or_default();
            let files: Vec<&str> = patch
                .lines()
                .filter_map(|line| {
                    ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                        .iter()
                        .find_map(|marker| line.strip_prefix(marker))
                })
                .collect();
            if files.is_empty() {
                "<no files>".to_owned()
            } else {
                files.join(", ")
            }
        }
        _ => name.to_owned(),
    }
}

/// Run one call. The error text is what the model is told, so it says what to
/// do differently rather than only what went wrong.
pub fn execute(workspace: &Path, name: &str, arguments: &Value) -> Result<String, String> {
    match name {
        "bash" => {
            let command = arguments["command"]
                .as_str()
                .ok_or("bash requires a `command` string")?;
            let timeout = arguments["timeout_ms"]
                .as_u64()
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1, MAX_TIMEOUT_MS);
            bash(workspace, command, Duration::from_millis(timeout))
        }
        "apply_patch" => {
            let patch = arguments["patch"]
                .as_str()
                .ok_or("apply_patch requires a `patch` string")?;
            apply_patch(workspace, patch)
        }
        other => Err(format!("`{other}` is not a tool this session offers")),
    }
}

/// Run `command` under the workspace shell, merging its output.
///
/// The child gets its own process group so a command that spawns children
/// cannot outlive its deadline by hiding behind them.
fn bash(workspace: &Path, command: &str, timeout: Duration) -> Result<String, String> {
    let mut child = {
        let mut spawn = Command::new("sh");
        spawn
            .arg("-c")
            .arg(command)
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            spawn.process_group(0);
        }
        spawn.spawn().map_err(|error| error.to_string())?
    };
    let stdout = drain(child.stdout.take().expect("piped stdout is present"));
    let stderr = drain(child.stderr.take().expect("piped stderr is present"));

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                timed_out = true;
                stop(&child, false);
                let forced = Instant::now() + KILL_GRACE;
                break loop {
                    match child.try_wait().map_err(|error| error.to_string())? {
                        Some(status) => break Some(status),
                        None if Instant::now() >= forced => {
                            stop(&child, true);
                            break child.wait().ok();
                        }
                        None => std::thread::sleep(Duration::from_millis(20)),
                    }
                };
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };

    let mut output = stdout.join().unwrap_or_default();
    output.push_str(&stderr.join().unwrap_or_default());
    let mut text = tail(&output);
    if text.trim().is_empty() {
        text = "(no output)".to_owned();
    }
    if timed_out {
        return Err(format!(
            "{text}\n\nCommand exceeded its {}s deadline and was stopped",
            timeout.as_secs_f32().round()
        ));
    }
    match status.and_then(|status| status.code()) {
        Some(0) => Ok(text),
        Some(code) => Err(format!("{text}\n\nCommand exited with code {code}")),
        None => Err(format!("{text}\n\nCommand was terminated by a signal")),
    }
}

fn stop(child: &std::process::Child, force: bool) {
    #[cfg(unix)]
    let _ = Command::new("kill")
        .args([
            if force { "-KILL" } else { "-TERM" },
            "--",
            &format!("-{}", child.id()),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(not(unix))]
    {
        let _ = (child, force);
    }
}

/// Read a pipe to the end on its own thread: a command that fills the pipe
/// while nobody reads it blocks instead of finishing.
fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// Keep the last `MAX_OUTPUT_BYTES` of output, cut at a character boundary.
fn tail(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_owned();
    }
    let mut cut = text.len() - MAX_OUTPUT_BYTES;
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!(
        "[{} earlier bytes omitted]\n{}",
        cut,
        text.get(cut..).unwrap_or_default()
    )
}

/// One file's worth of a patch.
#[derive(Debug, Eq, PartialEq)]
enum Change {
    Add {
        path: String,
        body: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        moved: Option<String>,
        hunks: Vec<Hunk>,
    },
}

/// One contiguous edit: the lines to find, and what replaces them.
#[derive(Debug, Eq, PartialEq)]
struct Hunk {
    /// Context and removed lines, in file order — what must be there.
    pattern: Vec<String>,
    /// Context and added lines — what is left behind.
    replacement: Vec<String>,
    /// `*** End of File`: the pattern is anchored at the end of the file.
    at_end: bool,
}

fn apply_patch(workspace: &Path, patch: &str) -> Result<String, String> {
    let changes = parse(patch)?;
    let mut written = Vec::new();
    for change in &changes {
        match change {
            Change::Add { path, body } => {
                let target = confine(workspace, path)?;
                if target.exists() {
                    return Err(format!("{path} already exists; use *** Update File"));
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                std::fs::write(&target, body).map_err(|error| format!("{path}: {error}"))?;
                written.push(format!("added {path}"));
            }
            Change::Delete { path } => {
                let target = confine(workspace, path)?;
                std::fs::remove_file(&target).map_err(|error| format!("{path}: {error}"))?;
                written.push(format!("deleted {path}"));
            }
            Change::Update { path, moved, hunks } => {
                let target = confine(workspace, path)?;
                let current =
                    std::fs::read_to_string(&target).map_err(|error| format!("{path}: {error}"))?;
                let updated =
                    update(&current, hunks).map_err(|error| format!("{path}: {error}"))?;
                let destination = match moved {
                    Some(moved) => confine(workspace, moved)?,
                    None => target.clone(),
                };
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                std::fs::write(&destination, updated)
                    .map_err(|error| format!("{path}: {error}"))?;
                if destination != target {
                    std::fs::remove_file(&target).map_err(|error| format!("{path}: {error}"))?;
                    written.push(format!(
                        "updated {path} and moved it to {}",
                        moved.as_deref().unwrap_or_default()
                    ));
                } else {
                    written.push(format!("updated {path}"));
                }
            }
        }
    }
    Ok(written.join("\n"))
}

/// Resolve a patch path inside the workspace.
///
/// Two checks, because either one alone lets a file out. The lexical one
/// refuses `..` and absolute paths, and works on a file the patch is about to
/// create. The real one resolves the nearest existing ancestor and requires it
/// to be inside the workspace, which is what a symlink pointing out of the
/// tree fails — the lexical check cannot see it, because the name looks local.
fn confine(workspace: &Path, path: &str) -> Result<PathBuf, String> {
    let outside = || format!("{path} is outside the workspace");
    let candidate = Path::new(path);
    if candidate.is_absolute()
        || candidate
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(outside());
    }
    let root = workspace
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let target = root.join(candidate);
    let mut existing = target.as_path();
    let anchor = loop {
        match existing.canonicalize() {
            Ok(resolved) => break resolved,
            // Not created yet: ask the same question of its parent, which is
            // where a symlink out of the workspace would have to be.
            Err(_) => existing = existing.parent().ok_or_else(outside)?,
        }
    };
    if !anchor.starts_with(&root) {
        return Err(outside());
    }
    Ok(target)
}

fn parse(patch: &str) -> Result<Vec<Change>, String> {
    let mut lines = patch.lines().peekable();
    let begun = lines
        .next()
        .is_some_and(|line| line.trim() == "*** Begin Patch");
    if !begun {
        return Err("a patch starts with `*** Begin Patch`".to_owned());
    }
    let mut changes = Vec::new();
    while let Some(line) = lines.next() {
        if line.trim() == "*** End Patch" {
            return Ok(changes);
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let mut body = String::new();
            while lines.peek().is_some_and(|next| next.starts_with('+')) {
                body.push_str(&lines.next().expect("peeked")[1..]);
                body.push('\n');
            }
            changes.push(Change::Add {
                path: path.trim().to_owned(),
                body,
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            changes.push(Change::Delete {
                path: path.trim().to_owned(),
            });
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            let mut moved = None;
            if let Some(destination) = lines
                .peek()
                .and_then(|next| next.strip_prefix("*** Move to: "))
            {
                moved = Some(destination.trim().to_owned());
                lines.next();
            }
            let mut hunks: Vec<Hunk> = Vec::new();
            while let Some(next) = lines.peek() {
                if next.starts_with("*** ") && next.trim() != "*** End of File" {
                    break;
                }
                let next = lines.next().expect("peeked");
                if next.trim() == "*** End of File" {
                    if let Some(hunk) = hunks.last_mut() {
                        hunk.at_end = true;
                    }
                    continue;
                }
                // `@@` opens a hunk. Its text is a hint for a human reader; the
                // lines that follow are what has to match.
                if next.starts_with("@@") {
                    hunks.push(Hunk {
                        pattern: Vec::new(),
                        replacement: Vec::new(),
                        at_end: false,
                    });
                    continue;
                }
                let hunk = match hunks.last_mut() {
                    Some(hunk) => hunk,
                    None => {
                        hunks.push(Hunk {
                            pattern: Vec::new(),
                            replacement: Vec::new(),
                            at_end: false,
                        });
                        hunks.last_mut().expect("just pushed")
                    }
                };
                match next.chars().next() {
                    Some('+') => hunk.replacement.push(next[1..].to_owned()),
                    Some('-') => hunk.pattern.push(next[1..].to_owned()),
                    Some(' ') => {
                        hunk.pattern.push(next[1..].to_owned());
                        hunk.replacement.push(next[1..].to_owned());
                    }
                    // A blank line inside a hunk is an empty context line that
                    // lost its space in transit.
                    None => {
                        hunk.pattern.push(String::new());
                        hunk.replacement.push(String::new());
                    }
                    Some(_) => return Err(format!("`{next}` is not a patch line")),
                }
            }
            if hunks
                .iter()
                .all(|hunk| hunk.pattern.is_empty() && hunk.replacement.is_empty())
            {
                return Err(format!("the update of {} has no changes", path.trim()));
            }
            changes.push(Change::Update {
                path: path.trim().to_owned(),
                moved,
                hunks,
            });
        } else if !line.trim().is_empty() {
            return Err(format!("`{line}` is not a patch header"));
        }
    }
    Err("the patch is missing `*** End Patch`".to_owned())
}

fn update(current: &str, hunks: &[Hunk]) -> Result<String, String> {
    let trailing = current.ends_with('\n');
    let mut lines: Vec<String> = current.lines().map(str::to_owned).collect();
    let mut cursor = 0;
    for hunk in hunks {
        let at = seek(&lines, &hunk.pattern, cursor, hunk.at_end).ok_or_else(|| {
            format!(
                "no line matched the context near `{}`",
                hunk.pattern.first().map_or("", String::as_str)
            )
        })?;
        lines.splice(at..at + hunk.pattern.len(), hunk.replacement.clone());
        // Later hunks apply after this one: a pattern is never matched inside
        // text an earlier hunk already replaced.
        cursor = at + hunk.replacement.len();
    }
    let mut out = lines.join("\n");
    if trailing && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// Find `pattern` in `lines` at or after `start`, loosening on whitespace.
///
/// Exact first, then ignoring trailing whitespace, then ignoring both ends: a
/// model that reflows indentation should not have its edit rejected, but the
/// order means an exact match is never passed over for a looser one.
fn seek(lines: &[String], pattern: &[String], start: usize, at_end: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(if at_end { lines.len() } else { start });
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let comparisons: [fn(&str, &str) -> bool; 3] = [
        |a, b| a == b,
        |a, b| a.trim_end() == b.trim_end(),
        |a, b| a.trim() == b.trim(),
    ];
    let last = lines.len() - pattern.len();
    for same in comparisons {
        // An end-anchored hunk is tried at the tail first, which is what
        // `*** End of File` asks for.
        let candidates = at_end.then_some(last).into_iter().chain(start..=last);
        for at in candidates {
            if lines[at..at + pattern.len()]
                .iter()
                .zip(pattern)
                .all(|(line, want)| same(line, want))
            {
                return Some(at);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temporary directory")
    }

    #[test]
    fn bash_returns_output_exit_codes_and_stops_a_command_that_overruns() {
        let root = workspace();
        let result = execute(root.path(), "bash", &json!({"command": "printf 'hi\\n'"}));
        assert_eq!(result.as_deref(), Ok("hi\n"));

        // stderr is merged, and a non-zero exit is an error the model can read.
        let failed = execute(
            root.path(),
            "bash",
            &json!({"command": "printf 'oops\\n' >&2; exit 3"}),
        )
        .unwrap_err();
        assert!(failed.contains("oops"), "{failed}");
        assert!(failed.ends_with("Command exited with code 3"), "{failed}");

        // The workspace is the working directory, not the process's cwd.
        std::fs::write(root.path().join("marker"), "x").unwrap();
        assert!(execute(root.path(), "bash", &json!({"command": "ls"}))
            .unwrap()
            .contains("marker"));

        // A command that ignores the deadline is stopped rather than waited on.
        let started = Instant::now();
        let timed_out = execute(
            root.path(),
            "bash",
            &json!({"command": "trap '' TERM; sleep 30", "timeout_ms": 300}),
        )
        .unwrap_err();
        assert!(timed_out.contains("deadline"), "{timed_out}");
        assert!(started.elapsed() < Duration::from_secs(5));

        // Output beyond the cap keeps the tail, which is where a failure is.
        let long = execute(
            root.path(),
            "bash",
            &json!({"command": "seq 1 100000; printf 'LAST\\n'"}),
        )
        .unwrap();
        assert!(long.len() < MAX_OUTPUT_BYTES + 128, "{}", long.len());
        assert!(long.contains("earlier bytes omitted"), "{long}");
        assert!(long.ends_with("LAST\n"), "{long}");
    }

    #[test]
    fn apply_patch_adds_updates_moves_and_deletes_inside_the_workspace() {
        let root = workspace();
        let added = execute(
            root.path(),
            "apply_patch",
            &json!({"patch": "*** Begin Patch\n*** Add File: src/main.rs\n+fn main() {\n+    println!(\"one\");\n+}\n*** End Patch\n"}),
        )
        .unwrap();
        assert_eq!(added, "added src/main.rs");
        assert_eq!(
            std::fs::read_to_string(root.path().join("src/main.rs")).unwrap(),
            "fn main() {\n    println!(\"one\");\n}\n"
        );

        // Context anchors the change: the `-` line is found by the lines around it.
        execute(
            root.path(),
            "apply_patch",
            &json!({"patch": "*** Begin Patch\n*** Update File: src/main.rs\n@@ fn main\n fn main() {\n-    println!(\"one\");\n+    println!(\"two\");\n }\n*** End Patch\n"}),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("src/main.rs")).unwrap(),
            "fn main() {\n    println!(\"two\");\n}\n"
        );

        // Indentation that drifted still matches, after the exact pass fails.
        execute(
            root.path(),
            "apply_patch",
            &json!({"patch": "*** Begin Patch\n*** Update File: src/main.rs\n-  println!(\"two\");\n+    println!(\"three\");\n*** End Patch\n"}),
        )
        .unwrap();
        assert!(std::fs::read_to_string(root.path().join("src/main.rs"))
            .unwrap()
            .contains("three"));

        // A move rewrites the file at its new path and leaves nothing behind.
        execute(
            root.path(),
            "apply_patch",
            &json!({"patch": "*** Begin Patch\n*** Update File: src/main.rs\n*** Move to: src/app.rs\n-}\n+}\n*** End Patch\n"}),
        )
        .unwrap();
        assert!(!root.path().join("src/main.rs").exists());
        assert!(root.path().join("src/app.rs").exists());

        let deleted = execute(
            root.path(),
            "apply_patch",
            &json!({"patch": "*** Begin Patch\n*** Delete File: src/app.rs\n*** End Patch\n"}),
        )
        .unwrap();
        assert_eq!(deleted, "deleted src/app.rs");
        assert!(!root.path().join("src/app.rs").exists());
    }

    #[test]
    fn a_patch_that_cannot_be_placed_is_refused_rather_than_guessed() {
        let root = workspace();
        std::fs::write(root.path().join("file.txt"), "alpha\nbeta\n").unwrap();

        let stale = execute(
            root.path(),
            "apply_patch",
            &json!({"patch": "*** Begin Patch\n*** Update File: file.txt\n-gamma\n+delta\n*** End Patch\n"}),
        )
        .unwrap_err();
        assert!(stale.contains("no line matched"), "{stale}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("file.txt")).unwrap(),
            "alpha\nbeta\n",
            "a rejected patch leaves the file alone"
        );

        // A symlink is a local-looking name that resolves out of the tree, so
        // the lexical check passes it and the resolved one has to catch it.
        let elsewhere = workspace();
        std::fs::write(elsewhere.path().join("target.txt"), "outside\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("link")).unwrap();
        #[cfg(unix)]
        {
            let escaped = execute(
                root.path(),
                "apply_patch",
                &json!({"patch": "*** Begin Patch\n*** Update File: link/target.txt\n-outside\n+captured\n*** End Patch\n"}),
            )
            .unwrap_err();
            assert!(escaped.contains("outside the workspace"), "{escaped}");
            assert_eq!(
                std::fs::read_to_string(elsewhere.path().join("target.txt")).unwrap(),
                "outside\n"
            );
        }

        for escape in ["../outside.txt", "/etc/passwd"] {
            let refused = execute(
                root.path(),
                "apply_patch",
                &json!({"patch": format!("*** Begin Patch\n*** Add File: {escape}\n+x\n*** End Patch\n")}),
            )
            .unwrap_err();
            assert!(refused.contains("outside the workspace"), "{refused}");
        }

        let unmarked =
            execute(root.path(), "apply_patch", &json!({"patch": "just text"})).unwrap_err();
        assert!(unmarked.contains("*** Begin Patch"), "{unmarked}");

        let unknown = execute(root.path(), "search", &json!({})).unwrap_err();
        assert!(unknown.contains("not a tool"), "{unknown}");
    }

    #[test]
    fn the_offered_schemas_name_both_tools_and_their_required_arguments() {
        let offered = schemas();
        assert_eq!(
            offered
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["bash", "apply_patch"]
        );
        assert_eq!(offered[0].input_schema["required"], json!(["command"]));
        assert_eq!(offered[1].input_schema["required"], json!(["patch"]));
        assert_eq!(summarize("bash", &json!({"command": "ls -a"})), "ls -a");
        assert_eq!(
            summarize(
                "apply_patch",
                &json!({"patch": "*** Begin Patch\n*** Update File: a.rs\n*** Delete File: b.rs\n"})
            ),
            "a.rs, b.rs"
        );
    }
}
