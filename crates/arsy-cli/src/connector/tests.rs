use super::*;
use arsy_code::mcp::{Channel, McpError};
use arsy_kernel::config::CompatSeed;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Condvar,
    },
    time::Duration,
};

/// A server whose connection waits for the test to open a gate, so what is
/// offered while it connects can be observed.
struct Gate(Mutex<bool>, Condvar);

impl Gate {
    fn open(&self) {
        *self.0.lock().unwrap() = true;
        self.1.notify_all();
    }
}

struct Fake {
    gate: Arc<Gate>,
    opened: Arc<AtomicUsize>,
}

struct FakeServer;

impl Channel for FakeServer {
    fn request(&mut self, method: &str, _params: Value) -> Result<Value, McpError> {
        Ok(match method {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake", "version": "0"},
            }),
            "tools/list" => {
                json!({"tools": [{"name": "search", "inputSchema": {"type": "object"}}]})
            }
            other => return Err(McpError::Protocol(format!("unexpected {other}"))),
        })
    }

    fn notify(&mut self, _method: &str, _params: Value) -> Result<(), McpError> {
        Ok(())
    }

    fn close(&mut self) -> Result<(), McpError> {
        Ok(())
    }
}

impl ChannelFactory for Fake {
    fn connect(&self, definition: &McpServer) -> Result<Box<dyn Channel>, McpError> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        if definition.transport.target() == "missing" {
            return Err(McpError::Transport("cannot start `missing`".into()));
        }
        let mut open = self.gate.0.lock().unwrap();
        while !*open {
            open = self.gate.1.wait(open).unwrap();
        }
        Ok(Box::new(FakeServer))
    }
}

fn config(servers: &[(&str, &str, bool)]) -> Config {
    let seed = CompatSeed {
        label: "claude".to_owned(),
        mcp_servers: servers
            .iter()
            .map(|(name, command, enabled)| McpServer {
                name: (*name).to_owned(),
                transport: McpTransport::Stdio {
                    command: (*command).to_owned(),
                    args: Vec::new(),
                    env: Default::default(),
                },
                enabled: *enabled,
                trust: arsy_kernel::capability::PolicySource::User,
                timeout_ms: 1_000,
                max_body_bytes: 1024 * 1024,
            })
            .collect(),
        ..CompatSeed::default()
    };
    Config::load_with(&[], &[seed]).unwrap()
}

fn connector(cache: &Path, gate: &Arc<Gate>, opened: &Arc<AtomicUsize>) -> McpConnector {
    McpConnector::with_channels(
        Some(cache.to_path_buf()),
        Arc::new(Fake {
            gate: Arc::clone(gate),
            opened: Arc::clone(opened),
        }),
    )
}

fn names(tools: &[DynamicTool]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_str()).collect()
}

#[test]
fn a_server_connects_once_in_the_background_and_is_reused() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("mcp-tools.json");
    let (gate, opened) = (
        Arc::new(Gate(Mutex::new(false), Condvar::new())),
        Arc::default(),
    );
    let docs = config(&[("docs", "docs-mcp", true)]);

    let first = connector(&cache, &gate, &opened);
    assert!(
        first.sync(&docs).is_empty(),
        "nothing known yet, and the turn did not wait"
    );
    gate.open();
    first.pending.wait("docs", Duration::from_secs(5));
    assert_eq!(names(&first.sync(&docs)), ["mcp__docs__search"]);
    first.sync(&docs);
    assert_eq!(opened.load(Ordering::SeqCst), 1, "reused, not reconnected");

    // A later session offers the cached tools while the server connects.
    let (closed, reopened) = (
        Arc::new(Gate(Mutex::new(false), Condvar::new())),
        Arc::default(),
    );
    let second = connector(&cache, &closed, &reopened);
    assert_eq!(names(&second.sync(&docs)), ["mcp__docs__search"]);
    closed.open();

    // Switching it off disconnects it.
    first.sync(&config(&[("docs", "docs-mcp", false)]));
    assert!(first.connections.lock().unwrap().is_empty());
}

#[test]
fn a_failed_server_is_reported_once_and_retried_only_when_redefined() {
    let directory = tempfile::tempdir().unwrap();
    let (gate, opened) = (
        Arc::new(Gate(Mutex::new(true), Condvar::new())),
        Arc::default(),
    );
    let connector = connector(&directory.path().join("cache.json"), &gate, &opened);
    let broken = config(&[("pencil", "missing", true)]);

    connector.sync(&broken);
    connector.pending.wait("pencil", Duration::from_secs(5));
    assert_eq!(connector.failures().len(), 1);
    connector.sync(&broken);
    connector.sync(&broken);
    assert!(connector.failures().is_empty(), "reported once");
    assert_eq!(opened.load(Ordering::SeqCst), 1, "not retried every turn");

    connector.sync(&config(&[("pencil", "pencil-mcp", true)]));
    connector.pending.wait("pencil", Duration::from_secs(5));
    assert_eq!(
        opened.load(Ordering::SeqCst),
        2,
        "a new definition is tried"
    );
    assert!(connector.failures().is_empty());
}

#[test]
fn a_changed_launch_value_is_a_different_definition() {
    let server = |token: &str| McpServer {
        name: "docs".to_owned(),
        transport: McpTransport::Stdio {
            command: "docs-mcp".to_owned(),
            args: Vec::new(),
            env: BTreeMap::from([("TOKEN".to_owned(), token.to_owned())]).into(),
        },
        enabled: true,
        trust: arsy_kernel::capability::PolicySource::User,
        timeout_ms: 1_000,
        max_body_bytes: 1024,
    };
    let (one, two) = (fingerprint(&server("a")), fingerprint(&server("b")));
    assert_ne!(one, two);
    assert_eq!(one.len(), 64, "only a SHA-256 digest is kept");
}
