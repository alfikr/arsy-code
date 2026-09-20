//! The interactive pickers, dialogs, and slash-command routing of the TUI.
#[cfg(feature = "tui")]
pub(crate) mod dialog;
#[cfg(feature = "tui")]
pub(crate) mod prompt;
#[cfg(feature = "tui")]
pub(crate) mod remembered;
#[cfg(feature = "tui")]
pub(crate) mod session;
#[cfg(feature = "tui")]
pub(crate) mod wizard;
