//! Filesystem, search, edit, and execution capabilities for ARSY CODE.

pub mod benchmark;
pub mod compat;
#[cfg(feature = "dap")]
pub mod dap;
pub mod edit;
#[cfg(feature = "wasm")]
pub mod extension;
pub mod git;
pub mod intelligence;
pub mod lsp;
pub mod process;
pub mod resource;
pub mod review;
pub mod sandbox;
pub mod search;
pub mod shell;
pub mod syntax;
pub mod workspace;

/// Version shared with the domain kernel.
pub const VERSION: &str = arsy_kernel::VERSION;
