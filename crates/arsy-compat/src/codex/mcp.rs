//! MCP servers Codex declares in `config.toml`: the repository's
//! `.codex/config.toml` and the operator's `$CODEX_HOME/config.toml`.
//!
//! Codex does not expand placeholders. It names variables instead --
//! `env_vars`, `env_http_headers`, `bearer_token_env_var` -- which a repository
//! file may not use to put the operator's variables into a header.

use crate::{
    mcp::{empty_seed, place, required, string_map, strings, Context, Declared, Scope},
    read,
};
use arsy_kernel::config::{CompatSeed, McpTransport};
use serde_json::Value;
use std::path::Path;

pub(crate) fn seeds(context: &Context) -> Vec<CompatSeed> {
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
            server(context, scope, value),
        );
    }
    seed
}

fn server(context: &Context, scope: Scope, value: &Value) -> Result<Declared, String> {
    let mut problems = Vec::new();
    let transport = if value.get("url").is_some() {
        http(context, scope, value, &mut problems)?
    } else if value.get("command").is_some() {
        stdio(context, value)?
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

fn stdio(context: &Context, value: &Value) -> Result<McpTransport, String> {
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

fn http(
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
