//! `arsy serve`: speak a protocol on stdio for an embedding client.
//!
//! Never a background daemon: the loop lives and dies with the pipe it was
//! given, so closing the client's end is what stops it.
//!
//! The MCP surface is the one this build serves. It offers ARSY's operations as
//! tools, and every call goes through the same policy engine a local call does —
//! a client on the other end of a pipe holds no authority of its own.

use crate::{load_config, storage_failed, usage, Command, Diagnostic, Emitter, Invocation};
use arsy_code::mcp_server::McpServer;
use arsy_kernel::policy::{RiskContext, WorkspaceCleanliness};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};

/// Longest single message accepted from a client, matching the canonical
/// transport's own bound.
const MAX_MESSAGE_BYTES: usize = arsy_kernel::transport::MAX_MESSAGE_BYTES;

pub fn parse(arguments: &crate::ParsedArguments) -> Result<Command, Diagnostic> {
    if !arguments.positional.is_empty() {
        return Err(usage("serve takes no positional argument"));
    }
    match arguments.transport.as_deref() {
        None | Some("stdio") => {}
        Some(other) => {
            return Err(usage(format!(
                "--transport must be `stdio`, not `{other}`; serve is never a background daemon"
            )))
        }
    }
    match arguments.protocol.as_deref() {
        None | Some("mcp") => Ok(Command::Serve),
        Some("acp") => Ok(Command::ServeAcp),
        Some(other @ "canonical") => Err(Diagnostic::error(
            "ARSY-SCH-1002",
            format!("`arsy serve --protocol {other}` is not available yet"),
            "use `--protocol acp` for an editor, or `--protocol mcp` to offer operations as tools",
        )),
        Some(other) => Err(usage(format!(
            "--protocol must be `mcp` or `acp`, not `{other}`"
        ))),
    }
}

/// Serve MCP on stdio until the client closes its end.
pub fn run(invocation: &Invocation, _emitter: &mut Emitter) -> Result<i32, Diagnostic> {
    let root = crate::workspace_root(&invocation.workspace)?;
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let config = load_config(&root, &working)?;
    let workspace = arsy_code::resource::Workspace::open(&root)
        .map_err(|error| storage_failed(error.to_string()))?;
    let artifacts = std::sync::Arc::new(
        arsy_kernel::artifact::FileArtifactStore::open(root.join(".arsy/artifacts"), 0)
            .map_err(|error| storage_failed(error.to_string()))?,
    );
    // One registry for the life of this process, and this process is the
    // whole of one client's session: its own fresh scope is the plan and
    // validation history's correct lifetime.
    let registry = arsy_code::operations::registry(
        &workspace,
        artifacts,
        arsy_kernel::artifact::unix_time_ms(),
        arsy_code::operations::Reachable::from_config(&config),
        &arsy_kernel::domain::SessionId::new().to_string(),
    )
    .map_err(|error| storage_failed(error.to_string()))?;

    let server = McpServer::new(
        registry,
        config.policy_rule_set(),
        &root,
        crate::actor(),
        RiskContext {
            // A served call is not interactive, so nothing can be confirmed
            // mid-flight; the risk context says so and policy decides on it.
            reversible: false,
            workspace: arsy_code::git::cleanliness(&root).unwrap_or(WorkspaceCleanliness::Unknown),
            sandbox: crate::installed_sandbox_assurance(),
        },
    );

    // The protocol owns stdout: nothing here writes a machine record, so a
    // client never has to filter ARSY's own framing out of the stream.
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    loop {
        let Some(line) = read_line(&mut reader)? else {
            return Ok(0);
        };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(line.trim()) {
            Ok(message) => server.handle(&message),
            // A message that does not parse has no id to correlate against, so
            // the error is uncorrelated rather than attributed to a request the
            // client never made.
            Err(error) => Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": error.to_string()},
            })),
        };
        if let Some(answer) = answer {
            let encoded = serde_json::to_string(&answer).map_err(storage_failed)?;
            writeln!(writer, "{encoded}")
                .and_then(|()| writer.flush())
                .map_err(storage_failed)?;
        }
    }
}

/// One line, bounded. An unbounded read here would let a client grow this
/// process until it dies.
pub(crate) fn read_line(reader: &mut impl BufRead) -> Result<Option<String>, Diagnostic> {
    let mut buffer = Vec::new();
    let read = Read::take(reader, MAX_MESSAGE_BYTES as u64 + 1)
        .read_until(b'\n', &mut buffer)
        .map_err(storage_failed)?;
    if read == 0 {
        return Ok(None);
    }
    if buffer.len() > MAX_MESSAGE_BYTES {
        return Err(Diagnostic::error(
            "ARSY-PRT-1102",
            format!("a message exceeded {MAX_MESSAGE_BYTES} bytes"),
            "the client sent more than the transport accepts in one message",
        ));
    }
    String::from_utf8(buffer)
        .map(Some)
        .map_err(|_| usage("the client sent a message that is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&str]) -> Result<Command, Diagnostic> {
        crate::parse(args.iter().map(|argument| (*argument).to_owned())).map(|it| it.command)
    }

    #[test]
    fn serve_defaults_to_mcp_over_stdio_and_refuses_a_daemon() {
        assert_eq!(command(&["serve"]).unwrap(), Command::Serve);
        assert_eq!(
            command(&["serve", "--protocol", "mcp", "--transport", "stdio"]).unwrap(),
            Command::Serve
        );
        // A socket would be a background daemon, which this command is not.
        assert!(command(&["serve", "--transport", "socket"]).is_err());
        assert!(command(&["serve", "extra"]).is_err());
        assert!(command(&["serve", "--protocol", "smoke-signals"]).is_err());

        assert_eq!(
            command(&["serve", "--protocol", "acp"]).unwrap(),
            Command::ServeAcp
        );

        // An adapter that exists but is not served says which, rather than
        // failing as unknown input.
        let error = command(&["serve", "--protocol", "canonical"]).unwrap_err();
        assert_eq!(error.code, "ARSY-SCH-1002");
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn an_oversized_message_is_refused_without_buffering_it() {
        let huge = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        let error = read_line(&mut huge.as_slice()).expect_err("the bound holds");
        assert_eq!(error.code, "ARSY-PRT-1102");

        let mut input = "{}\n".as_bytes();
        assert_eq!(read_line(&mut input).unwrap().as_deref(), Some("{}\n"));
        assert!(read_line(&mut input).unwrap().is_none());
    }
}
