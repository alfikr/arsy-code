//! Pure domain types shared by ARSY components.

pub mod domain;
pub mod event;
pub mod sqlite;

/// Workspace package version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
