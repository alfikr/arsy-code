//! Which Markdown the model is actually told about.
//!
//! A repository is full of Markdown and almost none of it is an instruction.
//! Injecting every `.md` file would spend the turn's budget on release notes;
//! injecting none, which is what this harness did before, means a project's own
//! conventions never reach the model at all.
//!
//! So discovery is deliberate and narrow: only files at [`INSTRUCTION_NAMES`],
//! only on the path from the workspace root down to the working directory, root
//! first — the same ancestor walk `arsy integrations import` reports, so what
//! the model is told and what `arsy integrations explain` prints cannot drift.
//! Everything else is documentation the model can choose to read with
//! `search.text` and `fs.read`, which is the point of having those tools.

use crate::resource::Workspace;
use arsy_kernel::{
    domain::FragmentId,
    prompt::{
        self, CompiledPrompt, ModelFamily, PromptFragment, PromptFragmentKind, PromptStrategy,
        BUILT_IN_STRATEGIES,
    },
    secret::Redactor,
};
use std::path::{Path, PathBuf};

/// Instruction files, in the order they are looked for in one directory. The
/// first that exists wins, so a project that keeps both does not get two copies
/// of the same guidance.
pub const INSTRUCTION_NAMES: [&str; 4] = ["AGENTS.md", "CLAUDE.md", "GEMINI.md", ".arsy/AGENTS.md"];

/// The most one instruction file may contribute. A file past this is truncated
/// rather than dropped: a long CONTRIBUTING-style AGENTS.md still carries its
/// first, most important paragraphs.
pub const MAX_INSTRUCTION_BYTES: u64 = 64 * 1024;

/// One instruction file that was found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Instruction {
    /// Workspace-relative, so it is quotable back to the operator.
    pub path: String,
    pub text: String,
    pub truncated: bool,
}

/// Walk from the workspace root down to `working_directory`, collecting the
/// instruction file in each directory that has one.
///
/// Root first, so a nested `AGENTS.md` is read after — and therefore overrides —
/// the one above it, which is the precedence every harness in this family uses.
pub fn discover(workspace: &Workspace, working_directory: &Path) -> Vec<Instruction> {
    let mut found = Vec::new();
    for directory in ancestors(workspace.path(), working_directory) {
        for name in INSTRUCTION_NAMES {
            let relative = directory.join(name);
            let Ok(content) = workspace.read(&relative, MAX_INSTRUCTION_BYTES + 1) else {
                continue;
            };
            let truncated = content.bytes.len() as u64 > MAX_INSTRUCTION_BYTES;
            let mut bytes = content.bytes;
            if truncated {
                bytes.truncate(usize::try_from(MAX_INSTRUCTION_BYTES).unwrap_or(usize::MAX));
                // Cut back to a character boundary rather than emitting a
                // replacement character mid-word.
                while !bytes.is_empty() && std::str::from_utf8(&bytes).is_err() {
                    bytes.pop();
                }
            }
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            found.push(Instruction {
                path: relative.to_string_lossy().replace('\\', "/"),
                text,
                truncated,
            });
            break;
        }
    }
    found
}

/// Directories from `root` down to `working_directory`, inclusive.
///
/// A working directory outside the workspace yields the root alone: the walk
/// exists to pick up nested project conventions, and there are none to pick up
/// on a path this workspace does not contain.
fn ancestors(root: &Path, working_directory: &Path) -> Vec<PathBuf> {
    let Ok(relative) = working_directory
        .canonicalize()
        .unwrap_or_else(|_| working_directory.to_path_buf())
        .strip_prefix(root)
        .map(Path::to_path_buf)
    else {
        return vec![PathBuf::new()];
    };
    let mut directories = vec![PathBuf::new()];
    let mut current = PathBuf::new();
    for component in relative.components() {
        current.push(component);
        directories.push(current.clone());
    }
    directories
}

/// The harness's own instructions: what the tools are for and how to use them.
///
/// This is the stable half of the system prompt, so it is a constant rather
/// than something assembled per turn — a prefix that never varies is what a
/// provider can cache.
pub const HARNESS_INSTRUCTIONS: &str = concat!(
    "You are ARSY, a coding agent working inside a single workspace.\n\n",
    "Use the structured tools before reaching for `bash`:\n",
    "- `search.files` to find files by glob, `search.text` to find code by content.\n",
    "- `fs.read` before editing anything; read the region you intend to change.\n",
    "- `fs.edit` for a small change, `apply_patch` for several, `fs.write` only for a whole file.\n",
    "- `bash` for builds, tests, and anything the other tools cannot express.\n\n",
    "Paths are relative to the workspace root. A tool that fails explains why; ",
    "read the error and adjust rather than repeating the call unchanged.\n",
);

/// Build the system prompt for one turn.
///
/// Compilation goes through the kernel's prompt compiler rather than string
/// concatenation, so ordering is the family's, secrets are redacted on the way
/// out, and every segment is traceable to the fragment it came from.
pub fn system_prompt(
    family: ModelFamily,
    instructions: &[Instruction],
    task_context: Option<&str>,
    redactor: &Redactor,
    token_budget: u32,
) -> Result<CompiledPrompt, prompt::PromptError> {
    let mut fragments = vec![PromptFragment {
        id: FragmentId::new(),
        kind: PromptFragmentKind::StableInstruction,
        content: HARNESS_INSTRUCTIONS.to_owned(),
    }];
    for instruction in instructions {
        fragments.push(PromptFragment {
            id: FragmentId::new(),
            kind: PromptFragmentKind::Context,
            content: format!(
                "<project-instructions path=\"{}\"{}>\n{}\n</project-instructions>",
                instruction.path,
                if instruction.truncated {
                    " truncated=\"true\""
                } else {
                    ""
                },
                instruction.text.trim_end()
            ),
        });
    }
    if let Some(context) = task_context.filter(|text| !text.trim().is_empty()) {
        fragments.push(PromptFragment {
            id: FragmentId::new(),
            kind: PromptFragmentKind::PermissionState,
            content: context.to_owned(),
        });
    }
    let strategies: Vec<&dyn PromptStrategy> = BUILT_IN_STRATEGIES
        .iter()
        .map(|strategy| strategy as &dyn PromptStrategy)
        .collect();
    prompt::compile(family, fragments, &strategies, redactor, token_budget)
}

/// The prompt family a provider and model belong to.
///
/// Wrong-but-close is fine here — the families differ only in the order of
/// prompt sections — so an unknown model falls back to the GPT ordering rather
/// than refusing the turn.
pub fn family_for(provider: &str, model: &str) -> ModelFamily {
    let subject = format!("{provider} {model}").to_lowercase();
    if subject.contains("claude") || subject.contains("anthropic") {
        ModelFamily::Claude
    } else if subject.contains("gemini") || subject.contains("google") {
        ModelFamily::Gemini
    } else if subject.contains("qwen") || subject.contains("deepseek") {
        ModelFamily::QwenDeepseek
    } else if subject.contains("ollama") || subject.contains("llama") || subject.contains("local") {
        ModelFamily::Local
    } else {
        ModelFamily::Gpt
    }
}

/// The rendered prompt as one string, in the order the strategy chose.
pub fn render(compiled: &CompiledPrompt) -> String {
    compiled
        .segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}
