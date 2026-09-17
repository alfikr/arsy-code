//! MCP servers Claude Code and Codex declare, as ARSY connections.
//!
//! Sources, first declaration of a name first — a later one with the same
//! name is shadowed, even when the first is switched off:
//!
//! 1. `~/.claude.json` `projects["<root>"].mcpServers` — Claude's local scope
//! 2. `<root>/.mcp.json` — the repository's
//! 3. `~/.claude.json` `mcpServers` — Claude's user scope
//! 4. `<root>/.codex/config.toml` `[mcp_servers]` — the repository's
//! 5. `$CODEX_HOME/config.toml` `[mcp_servers]` — Codex's user scope
//!
//! A repository's server starts only where the operator vouched for the
//! checkout, and a repository file can never put the operator's variables into
//! a URL or a header, which would send them to a host the repository chose.

use crate::{Context, Scope};
use arsy_kernel::config::{
    CompatSeed, McpServer, McpTransport, DEFAULT_MCP_MAX_BODY_BYTES, DEFAULT_MCP_TIMEOUT_MS,
};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

/// A declaration read and translated, before trust decides whether it starts.
pub(crate) struct Declared {
    pub(crate) transport: McpTransport,
    pub(crate) enabled: bool,
    pub(crate) timeout_ms: Option<u64>,
    /// Why it cannot start as written; any at all leaves it switched off.
    pub(crate) problems: Vec<String>,
}

/// Every server both tools declare, one seed per file in precedence order.
pub fn mcp_seeds(context: &Context) -> Vec<CompatSeed> {
    let mut seeds = Vec::new();
    if context.claude {
        seeds.extend(crate::claude::mcp::seeds(context));
    }
    if context.codex {
        seeds.extend(crate::codex::mcp::seeds(context));
    }
    seeds
}

pub(crate) fn empty_seed(label: &str, path: &Path) -> CompatSeed {
    CompatSeed {
        label: label.to_owned(),
        path: path.to_path_buf(),
        ..CompatSeed::default()
    }
}

/// Add one translated declaration, switched off wherever it may not start yet,
/// and say why.
pub(crate) fn place(
    seed: &mut CompatSeed,
    context: &Context,
    scope: Scope,
    name: &str,
    declared: Result<Declared, String>,
) {
    let declared = match declared {
        Ok(declared) => declared,
        Err(reason) => {
            seed.notes.push(format!("`{name}` {reason}; not connected"));
            return;
        }
    };
    let untrusted = scope == Scope::Workspace && !context.trusted;
    if untrusted && declared.enabled {
        seed.notes.push(format!(
            "`{name}` is off until `{}` is a trusted project",
            context.root.display()
        ));
    }
    seed.notes.extend(
        declared
            .problems
            .iter()
            .map(|problem| format!("`{name}` is off: {problem}")),
    );
    seed.mcp_servers.push(McpServer {
        name: name.to_owned(),
        enabled: declared.enabled && declared.problems.is_empty() && !untrusted,
        transport: declared.transport,
        trust: scope.trust(),
        timeout_ms: declared.timeout_ms.unwrap_or(DEFAULT_MCP_TIMEOUT_MS),
        max_body_bytes: DEFAULT_MCP_MAX_BODY_BYTES,
    });
}

pub(crate) fn required(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("needs a non-empty `{key}`"))
}

pub(crate) fn strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

pub(crate) fn string_map(value: &Value, key: &str) -> BTreeMap<String, String> {
    value
        .get(key)
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, text)| text.as_str().map(|text| (name.clone(), text.to_owned())))
        .collect()
}

#[cfg(test)]
mod tests;
