//! Filesystem, search, edit, and execution capabilities for ARSY CODE.

pub mod edit;
pub mod process;
pub mod resource;
pub mod search;
pub mod shell;

/// Version shared with the domain kernel.
pub const VERSION: &str = arsy_kernel::VERSION;
