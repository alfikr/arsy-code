//! Which Markdown the model is actually told about.
//!
//! A repository is full of Markdown and almost none of it is an instruction.
//! Injecting every `.md` file would spend the turn's budget on release notes;
//! injecting none, which is what this harness did before, means a project's own
//! conventions never reach the model at all.
//!
//! So discovery is deliberate and narrow: only [`AGENTS_NAMES`], [`CLAUDE_NAME`],
//! and [`FALLBACK_NAME`],
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

/// The agents file of one directory: the first of these that exists. Codex
/// reads `AGENTS.override.md` in place of `AGENTS.md`, so ARSY does too.
pub const AGENTS_NAMES: [&str; 3] = ["AGENTS.override.md", "AGENTS.md", ".arsy/AGENTS.md"];

/// Read beside the agents file, as Claude Code reads it, when the Claude
/// source is switched on. A copy of the agents file — the common symlink — is
/// not read twice.
pub const CLAUDE_NAME: &str = "CLAUDE.md";

/// Read only in a directory with neither of the above.
pub const FALLBACK_NAME: &str = "GEMINI.md";

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
    /// The operator's own file, outside any repository.
    pub operator: bool,
}

/// Walk from the workspace root down to `working_directory`, collecting the
/// instruction files in each directory that has any.
///
/// Root first, so a nested `AGENTS.md` is read after — and therefore overrides —
/// the one above it, which is the precedence every harness in this family uses.
pub fn discover(workspace: &Workspace, working_directory: &Path) -> Vec<Instruction> {
    discover_with(workspace, working_directory, true)
}

/// [`discover`], with `CLAUDE.md` read only when `claude` is on.
pub fn discover_with(
    workspace: &Workspace,
    working_directory: &Path,
    claude: bool,
) -> Vec<Instruction> {
    ancestors(workspace.path(), working_directory)
        .into_iter()
        .flat_map(|directory| in_directory(workspace, &directory, claude))
        .collect()
}

/// The agents file and `CLAUDE.md` of one directory, or its fallback.
fn in_directory(workspace: &Workspace, directory: &Path, claude: bool) -> Vec<Instruction> {
    let agents = AGENTS_NAMES
        .iter()
        .find_map(|name| read(workspace, &directory.join(name)));
    let claude_file = claude
        .then(|| read(workspace, &directory.join(CLAUDE_NAME)))
        .flatten()
        .filter(|found| {
            agents
                .as_ref()
                .is_none_or(|agents| agents.text != found.text)
        });
    let found: Vec<Instruction> = agents.into_iter().chain(claude_file).collect();
    if !found.is_empty() {
        return found;
    }
    read(workspace, &directory.join(FALLBACK_NAME))
        .into_iter()
        .collect()
}

fn read(workspace: &Workspace, relative: &Path) -> Option<Instruction> {
    let content = workspace.read(relative, MAX_INSTRUCTION_BYTES + 1).ok()?;
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
    Some(Instruction {
        path: relative.to_string_lossy().replace('\\', "/"),
        text: String::from_utf8(bytes).ok()?,
        truncated,
        operator: false,
    })
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

pub const PLAN_MODE_INSTRUCTIONS: &str = concat!(
    "You are in Plan Mode. Inspect the existing implementation with read-only tools and do not modify project state. ",
    "Use the plan tools to keep an ordered, repository-specific implementation plan. ",
    "Before finishing, identify current behavior, affected files, the implementation approach, validation, and important constraints. ",
    "Do not execute the plan; end with a concrete plan for the operator to approve or revise.\n",
);

/// One installed extension the model may call.
///
/// `plugin.invoke` is a generic operation — it takes a plugin id and a string —
/// so its schema cannot say which plugins exist. Without a listing the model
/// has a tool it can never successfully call, because it would have to guess
/// an id. This is that listing, refreshed with the rest of the prompt at each
/// turn boundary, so a plugin installed mid-session becomes callable at the
/// next turn rather than at the next restart.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionTool {
    pub id: String,
    pub version: String,
    /// What the operator approved it to do, in the manifest's own words.
    pub capabilities: Vec<String>,
}

fn extension_listing(extensions: &[ExtensionTool]) -> Option<String> {
    if extensions.is_empty() {
        return None;
    }
    let mut listing =
        String::from("Installed plugins, callable with `plugin.invoke` by the id shown:\n");
    for extension in extensions {
        listing.push_str(&format!(
            "- `{}` (version {}){}\n",
            extension.id,
            extension.version,
            if extension.capabilities.is_empty() {
                String::new()
            } else {
                format!(", granted {}", extension.capabilities.join(", "))
            }
        ));
    }
    Some(listing)
}

/// Build the system prompt for one turn.
///
/// `recalled` is context the caller retrieved — what this workspace remembers,
/// most often — carried as a context fragment so it is ordered with the
/// repository's own instructions and budgeted against them rather than added
/// on top of whatever they cost.
///
/// Compilation goes through the kernel's prompt compiler rather than string
/// concatenation, so ordering is the family's, secrets are redacted on the way
/// out, and every segment is traceable to the fragment it came from.
pub fn system_prompt(
    family: ModelFamily,
    instructions: &[Instruction],
    extensions: &[ExtensionTool],
    recalled: Option<&str>,
    mode: super::ExecutionMode,
    redactor: &Redactor,
    token_budget: u32,
) -> Result<CompiledPrompt, prompt::PromptError> {
    let mut fragments = vec![PromptFragment {
        id: FragmentId::new(),
        kind: PromptFragmentKind::StableInstruction,
        content: HARNESS_INSTRUCTIONS.to_owned(),
    }];
    if let Some(listing) = extension_listing(extensions) {
        // A permission-state fragment rather than a stable one: what is
        // installed changes at a turn boundary, and a prefix the provider is
        // caching must not be the thing that changes under it.
        fragments.push(PromptFragment {
            id: FragmentId::new(),
            kind: PromptFragmentKind::PermissionState,
            content: listing,
        });
    }
    for instruction in instructions {
        let tag = if instruction.operator {
            "user-instructions"
        } else {
            "project-instructions"
        };
        fragments.push(PromptFragment {
            id: FragmentId::new(),
            kind: PromptFragmentKind::Context,
            content: format!(
                "<{tag} path=\"{}\"{}>\n{}\n</{tag}>",
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
    if mode == super::ExecutionMode::Plan {
        fragments.push(PromptFragment {
            id: FragmentId::new(),
            kind: PromptFragmentKind::PermissionState,
            content: PLAN_MODE_INSTRUCTIONS.to_owned(),
        });
    }
    if let Some(context) = recalled.filter(|text| !text.trim().is_empty()) {
        fragments.push(PromptFragment {
            id: FragmentId::new(),
            kind: PromptFragmentKind::Context,
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
