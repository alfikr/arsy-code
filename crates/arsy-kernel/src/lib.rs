//! Pure domain types shared by ARSY components.

pub mod artifact;
pub mod capability;
pub mod domain;
pub mod event;
pub mod migrate;
pub mod operation;
pub mod policy;
pub mod projection;
pub mod protocol;
pub mod provider;
pub mod service;
pub mod sqlite;
pub mod transport;

/// Workspace package version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
