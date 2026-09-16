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

use crate::{expand, read, CompatHomes};
use arsy_kernel::{
    capability::PolicySource,
    config::{
        CompatSeed, LaunchEnv, McpServer, McpTransport, DEFAULT_MCP_MAX_BODY_BYTES,
        DEFAULT_MCP_TIMEOUT_MS,
    },
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// What a live read needs to know about where it runs.
pub struct Context<'a> {
    pub homes: &'a CompatHomes,
    pub root: &'a Path,
    /// Whether the operator vouched for `root`.
    pub trusted: bool,
    /// `compat.claude.enabled` and `compat.codex.enabled`.
    pub claude: bool,
    pub codex: bool,
    /// The process environment, injected so a test never reads the real one.
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scope {
    User,
    Workspace,
}

impl Scope {
    const fn trust(self) -> PolicySource {
        match self {
            Self::User => PolicySource::User,
            Self::Workspace => PolicySource::Workspace,
        }
    }
}

/// A declaration read and translated, before trust decides whether it starts.
struct Declared {
    transport: McpTransport,
    enabled: bool,
    timeout_ms: Option<u64>,
    /// Why it cannot start as written; any at all leaves it switched off.
    problems: Vec<String>,
}

/// Every server both tools declare, one seed per file in precedence order.
pub fn mcp_seeds(context: &Context) -> Vec<CompatSeed> {
    let mut seeds = Vec::new();
    if context.claude {
        seeds.extend(claude_seeds(context));
    }
    if context.codex {
        seeds.extend(codex_seeds(context));
    }
    seeds
}

fn claude_seeds(context: &Context) -> Vec<CompatSeed> {
    let project_file = context.root.join(".mcp.json");
    let mut project = json_seed(
        context,
        "claude",
        &project_file,
        Scope::Workspace,
        &read::json(&project_file),
        |document| document.get("mcpServers"),
    );
    let disabled = disabled_project_servers(context);
    project
        .mcp_servers
        .iter_mut()
        .filter(|server| disabled.contains(&server.name))
        .for_each(|server| server.enabled = false);

    let Some(user_file) = &context.homes.claude_json else {
        return vec![project];
    };
    let user = read::json(user_file);
    let root = context.root.to_string_lossy().into_owned();
    vec![
        json_seed(
            context,
            "claude",
            user_file,
            Scope::User,
            &user,
            |document| document.get("projects")?.get(&root)?.get("mcpServers"),
        ),
        project,
        json_seed(
            context,
            "claude",
            user_file,
            Scope::User,
            &user,
            |document| document.get("mcpServers"),
        ),
    ]
}

/// `disabledMcpjsonServers` from the operator's Claude settings. It can only
/// switch a repository's server off, so it is honoured from any of the files.
fn disabled_project_servers(context: &Context) -> BTreeSet<String> {
    let files = [
        context
            .homes
            .claude_dir
            .as_ref()
            .map(|dir| dir.join("settings.json")),
        Some(context.root.join(".claude/settings.json")),
        Some(context.root.join(".claude/settings.local.json")),
    ];
    files
        .into_iter()
        .flatten()
        .filter_map(|path| read::json(&path).ok().flatten())
        .flat_map(|settings| strings(&settings, "disabledMcpjsonServers"))
        .collect()
}

fn json_seed(
    context: &Context,
    label: &str,
    path: &Path,
    scope: Scope,
    document: &Result<Option<Value>, String>,
    select: impl Fn(&Value) -> Option<&Value>,
) -> CompatSeed {
    let mut seed = empty_seed(label, path);
    let servers = match document {
        Err(error) => {
            seed.notes.push(format!("{} {error}", path.display()));
            return seed;
        }
        Ok(document) => document
            .as_ref()
            .and_then(&select)
            .and_then(Value::as_object),
    };
    for (name, value) in servers.into_iter().flatten() {
        place(
            &mut seed,
            context,
            scope,
            name,
            claude_server(context, scope, value),
        );
    }
    seed
}

fn codex_seeds(context: &Context) -> Vec<CompatSeed> {
    let mut seeds = vec![toml_seed(
        context,
        &context.root.join(".codex/config.toml"),
        Scope::Workspace,
    )];
    if let Some(directory) = &context.homes.codex_dir {
        seeds.push(toml_seed(
            context,
            &directory.join("config.toml"),
            Scope::User,
        ));
    }
    seeds
}

fn toml_seed(context: &Context, path: &Path, scope: Scope) -> CompatSeed {
    let mut seed = empty_seed("codex", path);
    let table = match read::toml(path) {
        Err(error) => {
            seed.notes.push(format!("{} {error}", path.display()));
            return seed;
        }
        Ok(table) => table,
    };
    let servers = table
        .as_ref()
        .and_then(|table| table.get("mcp_servers"))
        .and_then(|servers| serde_json::to_value(servers).ok());
    for (name, value) in servers.iter().filter_map(Value::as_object).flatten() {
        place(
            &mut seed,
            context,
            scope,
            name,
            codex_server(context, scope, value),
        );
    }
    seed
}

fn empty_seed(label: &str, path: &Path) -> CompatSeed {
    CompatSeed {
        label: label.to_owned(),
        path: path.to_path_buf(),
        ..CompatSeed::default()
    }
}

/// Add one translated declaration, switched off wherever it may not start yet,
/// and say why.
fn place(
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

fn claude_server(context: &Context, scope: Scope, value: &Value) -> Result<Declared, String> {
    let kind = match value.get("type").and_then(Value::as_str) {
        Some(kind) => kind,
        None if value.get("url").is_some() => "http",
        None => "stdio",
    };
    let mut problems = Vec::new();
    let transport = match kind {
        "stdio" => claude_stdio(context, value, &mut problems)?,
        "http" | "streamable-http" => claude_http(context, scope, value, &mut problems)?,
        "sse" => return Err("uses the SSE transport, which ARSY does not connect over".to_owned()),
        other => return Err(format!("declares the unknown transport `{other}`")),
    };
    Ok(Declared {
        transport,
        enabled: value
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        timeout_ms: None,
        problems,
    })
}

fn claude_stdio(
    context: &Context,
    value: &Value,
    problems: &mut Vec<String>,
) -> Result<McpTransport, String> {
    let command = required(value, "command")?;
    let mut expand = |text: &str| expanded(context, text, problems);
    Ok(McpTransport::Stdio {
        command: expand(&command),
        args: strings(value, "args")
            .iter()
            .map(|arg| expand(arg))
            .collect(),
        env: string_map(value, "env")
            .into_iter()
            .map(|(key, text)| (key, expand(&text)))
            .collect::<BTreeMap<_, _>>()
            .into(),
    })
}

fn claude_http(
    context: &Context,
    scope: Scope,
    value: &Value,
    problems: &mut Vec<String>,
) -> Result<McpTransport, String> {
    let url = required(value, "url")?;
    let headers = string_map(value, "headers");
    let reads_variables =
        expand::has_placeholder(&url) || headers.values().any(|text| expand::has_placeholder(text));
    if scope == Scope::Workspace && reads_variables {
        problems.push(
            "a repository file may not put the operator's variables into a URL or header"
                .to_owned(),
        );
        return Ok(McpTransport::Http {
            url,
            headers: LaunchEnv::default(),
        });
    }
    let mut expand = |text: &str| expanded(context, text, problems);
    Ok(McpTransport::Http {
        url: expand(&url),
        headers: headers
            .into_iter()
            .map(|(name, text)| (name, expand(&text)))
            .collect::<BTreeMap<_, _>>()
            .into(),
    })
}

fn codex_server(context: &Context, scope: Scope, value: &Value) -> Result<Declared, String> {
    let mut problems = Vec::new();
    let transport = if value.get("url").is_some() {
        codex_http(context, scope, value, &mut problems)?
    } else if value.get("command").is_some() {
        codex_stdio(context, value)?
    } else {
        return Err("names neither a `command` nor a `url`".to_owned());
    };
    let seconds = ["startup_timeout_sec", "tool_timeout_sec"]
        .iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_f64))
        .fold(None, |longest: Option<f64>, seconds| {
            Some(longest.map_or(seconds, |longest| longest.max(seconds)))
        });
    Ok(Declared {
        transport,
        enabled: value
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        timeout_ms: seconds
            .filter(|seconds| *seconds > 0.0)
            .map(|seconds| (seconds * 1000.0).ceil() as u64),
        problems,
    })
}

fn codex_stdio(context: &Context, value: &Value) -> Result<McpTransport, String> {
    let mut env = string_map(value, "env");
    // Named variables are forwarded as Codex forwards them: when set.
    env.extend(
        strings(value, "env_vars")
            .into_iter()
            .filter_map(|name| (context.env)(&name).map(|value| (name, value))),
    );
    Ok(McpTransport::Stdio {
        command: required(value, "command")?,
        args: strings(value, "args"),
        env: env.into(),
    })
}

fn codex_http(
    context: &Context,
    scope: Scope,
    value: &Value,
    problems: &mut Vec<String>,
) -> Result<McpTransport, String> {
    let url = required(value, "url")?;
    let mut headers = string_map(value, "http_headers");
    let from_env = string_map(value, "env_http_headers");
    let bearer = value.get("bearer_token_env_var").and_then(Value::as_str);
    if scope == Scope::Workspace && (bearer.is_some() || !from_env.is_empty()) {
        problems.push(
            "a repository file may not put the operator's variables into a header".to_owned(),
        );
        return Ok(McpTransport::Http {
            url,
            headers: headers.into(),
        });
    }
    headers.extend(
        from_env
            .into_iter()
            .filter_map(|(header, name)| (context.env)(&name).map(|value| (header, value))),
    );
    if let Some(name) = bearer {
        match (context.env)(name) {
            Some(token) => {
                headers.insert("authorization".to_owned(), format!("Bearer {token}"));
            }
            None => problems.push(format!("`{name}` is not set")),
        }
    }
    Ok(McpTransport::Http {
        url,
        headers: headers.into(),
    })
}

fn expanded(context: &Context, text: &str, problems: &mut Vec<String>) -> String {
    expand::expand(text, context.env).unwrap_or_else(|name| {
        problems.push(format!("`{name}` is not set"));
        text.to_owned()
    })
}

fn required(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("needs a non-empty `{key}`"))
}

fn strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn string_map(value: &Value, key: &str) -> BTreeMap<String, String> {
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
