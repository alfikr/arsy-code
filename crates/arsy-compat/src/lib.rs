//! Claude Code and Codex, read live.
//!
//! Another tool's configuration is read from where that tool keeps it and
//! translated here into ARSY's own types, so its formats never reach the kernel
//! (invariant 5). Everything this crate returns sits below every `arsy.json`
//! layer, and nothing it reads from a repository can grant authority.

mod expand;
mod homes;
pub mod mcp;
mod read;

pub use homes::CompatHomes;
