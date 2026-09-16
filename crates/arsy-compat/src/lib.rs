//! Claude Code and Codex, read live.
//!
//! Another tool's configuration is read from where that tool keeps it and
//! translated here into ARSY's own types, so its formats never reach the kernel
//! (invariant 5). Everything this crate returns sits below every `arsy.json`
//! layer, and nothing it reads from a repository can grant authority.
//!
//! Layout: one module per tool (`claude/`, `codex/`), holding one file per
//! concern such as `mcp.rs`, and only that tool's format. What is shared --
//! where the homes are, bounded reads, and the precedence and trust every
//! tool's declarations go through -- lives at the crate root. Supporting
//! another tool means adding a folder of the same shape.

mod claude;
mod codex;
mod context;
mod homes;
pub mod instructions;
pub mod mcp;
mod model;
mod permissions;
mod read;

pub use context::Context;
pub(crate) use context::Scope;
pub use homes::CompatHomes;

use arsy_kernel::config::CompatSeed;

/// Everything Claude Code and Codex contribute to configuration, one seed per
/// file, in the order the kernel places them.
pub fn seeds(context: &Context) -> Vec<CompatSeed> {
    let mut seeds = mcp::mcp_seeds(context);
    seeds.extend(permissions::seeds(context));
    seeds.extend(model::seeds(context));
    seeds
}
