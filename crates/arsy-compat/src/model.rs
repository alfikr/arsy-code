//! The model Claude Code and Codex are set to use, as a fallback.
//!
//! Most specific first: Claude's local and project settings, then the
//! operator's own, then Codex's project and user configuration.

use crate::Context;
use arsy_kernel::config::CompatSeed;

pub(crate) fn seeds(context: &Context) -> Vec<CompatSeed> {
    let mut seeds = Vec::new();
    if context.claude {
        seeds.extend(crate::claude::model::seeds(context));
    }
    if context.codex {
        seeds.extend(crate::codex::model::seeds(context));
    }
    seeds
}
