//! `debug.run` against an adapter process, on Unix.
//!
//! The adapter is `sh` replaying framed DAP messages: the executor spawns it,
//! speaks the protocol to it, and has to reach the same answer it would from a
//! real debugger. What that proves is the part this crate owns — framing,
//! correlation, waiting for `stopped`, and turning frames and scopes into an
//! answer — against a process rather than against a mock in the same address
//! space.
//!
//! A real adapter (`debugpy`, `codelldb`) is not assumed: none is installed on
//! a build machine by definition, and a test that skipped itself when one was
//! missing would pass by not running.

#![cfg(all(unix, feature = "dap"))]

use arsy_code::{agent, operations, resource::Workspace};
use arsy_kernel::{
    artifact::{ArtifactStore, FileArtifactStore},
    capability::{CapabilityAction, PolicySource, ResourcePattern},
    domain::{OperationId, Principal, ResourceRef},
    operation::{OperationKind, OperationRequest},
    policy::{
        ActorMatch, PolicyRule, RiskContext, RuleEffect, RuleSet, SandboxAssurance,
        WorkspaceCleanliness,
    },
};
use serde_json::{json, Value};
use std::{sync::Arc, time::Instant};

/// One DAP message with its `Content-Length` header, as a shell `printf`
/// argument.
fn framed(message: Value) -> String {
    let body = message.to_string();
    format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

/// An adapter that answers the executor's requests in the order it makes them
/// and reports a breakpoint hit after `continue`.
fn adapter_script() -> String {
    let messages = [
        // 1 initialize
        json!({"type": "response", "request_seq": 1, "success": true, "body": {
            "supportsConfigurationDoneRequest": true,
            "supportsTerminateRequest": true
        }}),
        // 2 launch, 3 setBreakpoints, 4 configurationDone, 5 continue
        json!({"type": "response", "request_seq": 2, "success": true, "body": {}}),
        json!({"type": "response", "request_seq": 3, "success": true, "body": {
            "breakpoints": [{"verified": true, "line": 12}]
        }}),
        json!({"type": "response", "request_seq": 4, "success": true, "body": {}}),
        json!({"type": "response", "request_seq": 5, "success": true, "body": {"allThreadsContinued": true}}),
        // The program runs, prints, and stops -- after the continue was
        // answered, which is the ordering a real adapter uses.
        json!({"type": "event", "event": "output", "body": {"output": "starting\n"}}),
        json!({"type": "event", "event": "stopped", "body": {"reason": "breakpoint", "threadId": 7}}),
        // 6 stackTrace, 7 scopes, 8 variables, 9 evaluate, 10 terminate
        json!({"type": "response", "request_seq": 6, "success": true, "body": {"stackFrames": [
            {"id": 1000, "name": "divide", "line": 12},
            {"id": 1001, "name": "main", "line": 40}
        ]}}),
        json!({"type": "response", "request_seq": 7, "success": true, "body": {"scopes": [
            {"name": "Locals", "variablesReference": 5}
        ]}}),
        json!({"type": "response", "request_seq": 8, "success": true, "body": {"variables": [
            {"name": "divisor", "value": "0"},
            {"name": "total", "value": "42"}
        ]}}),
        json!({"type": "response", "request_seq": 9, "success": true, "body": {"result": "0"}}),
        json!({"type": "response", "request_seq": 10, "success": true, "body": {}}),
    ];
    let payload: String = messages.into_iter().map(framed).collect();
    // `printf %s` rather than `echo`, so nothing is added and no escape is
    // interpreted; the payload contains no single quotes.
    format!("printf %s '{payload}'; cat >/dev/null")
}

fn run(input: Value) -> Result<Value, String> {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(root.path()).unwrap();
    let artifacts: Arc<dyn ArtifactStore> =
        Arc::new(FileArtifactStore::open(root.path().join(".arsy/art"), 0).unwrap());
    // The same runtime a turn uses, with a rule that allows a debug launch:
    // the point is that `debug.run` goes through policy like anything else.
    let runtime = agent::runtime(
        &workspace,
        RuleSet::compile([PolicyRule {
            source: PolicySource::User,
            effect: RuleEffect::Allow,
            actor: ActorMatch::Any,
            action: CapabilityAction::DebugLaunch,
            pattern: ResourcePattern::new(CapabilityAction::DebugLaunch.default_scheme(), "**")
                .unwrap(),
            expires_at_ms: None,
            delegation_depth: 0,
            minimum_assurance: SandboxAssurance::None,
        }]),
        artifacts,
        0,
        Principal::User("tester".into()),
        RiskContext {
            reversible: true,
            workspace: WorkspaceCleanliness::Clean,
            sandbox: SandboxAssurance::None,
        },
        arsy_code::operations::Reachable::default(),
        "test",
    )
    .unwrap();

    // The contract comes from a registry built the same way the runtime builds
    // its own: what a call requires is a property of the operation, not of who
    // is asking.
    let registry = operations::registry(
        &workspace,
        Arc::new(FileArtifactStore::open(root.path().join(".arsy/art"), 0).unwrap()),
        0,
        arsy_code::operations::Reachable::default(),
        "test",
    )
    .unwrap();
    let kind = OperationKind::new("debug.run").unwrap();
    let contract = registry
        .contract(&kind)
        .expect("debug.run is registered in a build with the feature");
    let request = OperationRequest {
        id: OperationId::new(),
        kind,
        actor: Principal::User("tester".into()),
        requirements: operations::requirements(contract, &input, root.path()),
        input,
    };
    let grants = runtime
        .authorize(&request)
        .approve()
        .expect("the rule allows the launch");
    let result = runtime.dispatch("debug.run", &request, &grants, Instant::now());
    if result.success {
        Ok(result.metadata)
    } else {
        Err(result.output)
    }
}

#[test]
fn a_breakpoint_run_reports_where_it_stopped_and_what_was_in_scope() {
    let found = run(json!({
        "adapter": ["sh", "-c", adapter_script()],
        "launch": {"program": "app"},
        "breakpoints": [{"path": "src/main.rs", "lines": [12]}],
        "expressions": ["divisor"],
    }))
    .expect("the run completes");

    assert_eq!(found["stopped"], "breakpoint");
    assert_eq!(found["thread"], 7);
    assert_eq!(found["frames"][0]["name"], "divide");
    assert_eq!(found["frames"][0]["line"], 12);
    assert_eq!(found["variables"]["Locals"][0]["name"], "divisor");
    assert_eq!(found["variables"]["Locals"][0]["value"], "0");
    // The expression was evaluated in the frame that stopped, not guessed at.
    assert_eq!(found["evaluated"]["divisor"], "0");
    // What the program printed is reported, because a debugger run that
    // dropped the program's own output would be worse than a print statement.
    assert_eq!(found["output"], "starting\n");
    // Every exchange is an artifact, so the answer can be checked against the
    // conversation that produced it.
    assert!(
        found["exchanges"].as_array().unwrap().len() >= 8,
        "{found:#?}"
    );
}

#[test]
fn an_adapter_that_cannot_be_started_is_a_failed_operation_not_a_panic() {
    let error = run(json!({
        "adapter": ["./no-such-debug-adapter"],
        "launch": {},
    }))
    .expect_err("a missing adapter fails the operation");

    assert!(error.contains("no-such-debug-adapter"), "{error}");
}

#[test]
fn a_program_that_never_stops_reports_that_rather_than_inventing_a_stack() {
    // The program runs to completion: the adapter answers launch and continue,
    // then reports that the debuggee is gone. No `stopped` is ever sent, and
    // the run must not wait out its deadline to notice.
    let quiet = [
        json!({"type": "response", "request_seq": 1, "success": true, "body": {}}),
        json!({"type": "response", "request_seq": 2, "success": true, "body": {}}),
        json!({"type": "response", "request_seq": 3, "success": true, "body": {}}),
        json!({"type": "event", "event": "terminated"}),
    ];
    let payload: String = quiet.into_iter().map(framed).collect();

    let found = run(json!({
        "adapter": ["sh", "-c", format!("printf %s '{payload}'; cat >/dev/null")],
        "launch": {},
    }))
    .expect("the run completes");

    assert_eq!(found["stopped"], Value::Null);
    assert!(found["frames"].as_array().unwrap().is_empty());
    assert!(found["variables"].as_object().unwrap().is_empty());
}

/// The resource a debug launch is checked against, so a rule can name the
/// adapter rather than every debugger at once.
#[test]
fn the_launch_is_checked_against_the_adapter_it_would_run() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(root.path()).unwrap();
    let artifacts: Arc<dyn ArtifactStore> =
        Arc::new(FileArtifactStore::open(root.path().join(".arsy/art"), 0).unwrap());
    let registry = operations::registry(
        &workspace,
        artifacts,
        0,
        operations::Reachable::default(),
        "test",
    )
    .unwrap();
    let kind = OperationKind::new("debug.run").unwrap();
    let contract = registry.contract(&kind).unwrap();

    let requirements = operations::requirements(
        contract,
        &json!({"adapter": ["debugpy"], "launch": {}}),
        root.path(),
    );

    assert_eq!(requirements.len(), 1);
    assert_eq!(requirements[0].action, CapabilityAction::DebugLaunch);
    // Every debug launch is one resource today; naming the adapter is what a
    // rule would need to distinguish them, and this asserts what the current
    // mapping actually is rather than what it should become.
    assert_eq!(
        requirements[0].resource,
        ResourceRef::new("debug", "*").unwrap()
    );
}
