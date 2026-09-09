//! Running a program under a debugger once, and reporting what it was doing
//! when it stopped.
//!
//! # Why one operation instead of a debugging session
//!
//! An interactive debugger is a conversation: break, continue, step, inspect,
//! step again. Exposing that as operations would mean a debug session living
//! across tool calls, with its own lifetime, its own lease, and its own
//! failure modes — a second runtime beside the one that already exists.
//!
//! The question an agent actually has is smaller and answerable in one round
//! trip: *stop here and tell me what the values are*. So `debug.run` launches
//! the adapter, sets the breakpoints, lets the program run, and reports the
//! stack and the locals wherever it stopped. The adapter dies with the
//! operation.
//!
//! That is the print-debugging replacement: no edit to the program, no rebuild,
//! and the values come back as evidence rather than as text scraped from
//! stdout.

use crate::{
    dap::{DapHost, DebugEvidence, DebugOperation, DebugStart, StdioDapTransport},
    resource::Workspace,
};
use arsy_kernel::{
    artifact::{ArtifactStore, NewArtifact, Sensitivity},
    capability::{CapabilityAction, CapabilityGrant},
    domain::{Principal, ResourceRef},
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

/// How long the adapter has to answer any one request.
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
/// How long the whole run may take before it is abandoned.
const RUN_DEADLINE_MS: u64 = 120_000;
/// How long to wait for the program to reach a breakpoint before deciding it
/// never will.
const STOP_DEADLINE: Duration = Duration::from_secs(30);
/// Frames reported. Deeper than this is a stack trace, not an answer.
const MAX_FRAMES: usize = 20;
/// Variables reported per scope, for the same reason.
const MAX_VARIABLES: usize = 50;

#[derive(Debug, Default, Serialize)]
struct DebugRun {
    /// Why execution stopped: `breakpoint`, `exception`, `step`, or whatever
    /// the adapter called it. Absent when it never stopped.
    stopped: Option<String>,
    /// The thread that stopped, as the adapter numbers them.
    thread: Option<i64>,
    frames: Vec<Value>,
    /// Variables of the top frame's scopes, by scope name.
    variables: BTreeMap<String, Vec<Value>>,
    /// What each requested expression evaluated to where it stopped.
    evaluated: BTreeMap<String, String>,
    /// Every DAP exchange, as artifacts, in order.
    exchanges: Vec<String>,
    /// The program's own output, when the adapter reported any.
    output: String,
}

pub struct DebugExecutor {
    contract: OperationContract,
    workspace: PathBuf,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
}

impl DebugExecutor {
    pub fn new(
        workspace: &Workspace,
        artifacts: Arc<dyn ArtifactStore>,
        retain_until_ms: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            contract: OperationContract {
                kind: OperationKind::new("debug.run").expect("static operation kind is valid"),
                input_schema: InputSchema {
                    required: BTreeMap::from([
                        ("adapter".to_owned(), JsonType::Array),
                        ("launch".to_owned(), JsonType::Object),
                    ]),
                    optional: BTreeMap::from([
                        ("breakpoints".to_owned(), JsonType::Array),
                        ("expressions".to_owned(), JsonType::Array),
                    ]),
                    allow_extra: false,
                },
                actions: vec![CapabilityAction::DebugLaunch],
                // Running a program under a debugger runs the program.
                idempotency: Idempotency::Effectful,
                reversible: false,
                concurrency: ConcurrencyRule::ExclusiveGlobal,
            },
            workspace: workspace.path().to_owned(),
            artifacts,
            retain_until_ms,
        })
    }

    fn capture(&self, actor: &Principal, value: &Value) -> Result<ResourceRef, OperationError> {
        super::store(
            self.artifacts.as_ref(),
            value,
            actor.clone(),
            self.retain_until_ms,
        )
    }
}

/// Stores each DAP exchange as an artifact, so a debugging answer can be
/// checked against the conversation that produced it.
struct Evidence<'a> {
    artifacts: &'a dyn ArtifactStore,
    actor: Principal,
    retain_until_ms: u64,
}

impl crate::dap::DebugEvidenceSink for Evidence<'_> {
    fn capture(&mut self, value: &Value) -> Result<ResourceRef, crate::dap::DapError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| crate::dap::DapError::Transport(error.to_string()))?;
        self.artifacts
            .put(
                &bytes,
                NewArtifact {
                    media_type: "application/json".into(),
                    creator: self.actor.clone(),
                    source_revision: None,
                    sensitivity: Sensitivity::Internal,
                    retain_until_ms: self.retain_until_ms,
                },
            )
            .map(|metadata| metadata.resource_ref())
            .map_err(|error| crate::dap::DapError::Transport(error.to_string()))
    }
}

impl OperationExecutor for DebugExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let argv: Vec<String> = request
            .input
            .get("adapter")
            .and_then(Value::as_array)
            .map(|argv| {
                argv.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if argv.is_empty() {
            return Err(OperationError::Schema(
                "`adapter` must name the debug adapter to run, program first".to_owned(),
            ));
        }
        let launch = request
            .input
            .get("launch")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let transport = StdioDapTransport::spawn(&argv, REQUEST_DEADLINE).map_err(dap)?;
        let mut host = DapHost::connect(
            transport,
            Evidence {
                artifacts: self.artifacts.as_ref(),
                actor: request.actor.clone(),
                retain_until_ms: self.retain_until_ms,
            },
        )
        .map_err(dap)?;

        let mut run = DebugRun::default();
        let now = arsy_kernel::artifact::unix_time_ms();
        let deadline = now.saturating_add(RUN_DEADLINE_MS);
        fn note(run: &mut DebugRun, evidence: DebugEvidence) {
            run.exchanges.push(evidence.artifact.value().to_owned());
        }

        // The debuggee must not start before its breakpoints are in, so the
        // adapter is launched, configured, and only then told to go.
        note(
            &mut run,
            host.start(
                DebugStart::Launch,
                launch,
                &BTreeSet::from([CapabilityAction::DebugLaunch]),
                deadline,
            )
            .map_err(dap)?,
        );
        for breakpoint in breakpoints(&request.input, &self.workspace) {
            note(
                &mut run,
                host.operate(DebugOperation::SetBreakpoints(breakpoint), now)
                    .map_err(dap)?,
            );
        }
        if host.capabilities().configuration_done {
            note(
                &mut run,
                host.operate(DebugOperation::ConfigurationDone(json!({})), now)
                    .map_err(dap)?,
            );
        }
        note(
            &mut run,
            host.operate(DebugOperation::Continue(json!({"threadId": 1})), now)
                .map_err(dap)?,
        );

        // Everything below depends on the program having stopped somewhere. An
        // adapter answers `continue` at once and reports `stopped` when the
        // program actually stops, so this waits for it rather than looking at
        // what had already arrived. A program that runs to completion never
        // sends one, and saying so is the answer.
        if let Some(event) = host
            .wait_for_event(&["stopped", "terminated", "exited"], STOP_DEADLINE)
            .map_err(dap)?
        {
            if event.get("event").and_then(Value::as_str) == Some("stopped") {
                let body = event.get("body").cloned().unwrap_or(Value::Null);
                run.stopped = Some(
                    body.get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("stopped")
                        .to_owned(),
                );
                run.thread = body.get("threadId").and_then(Value::as_i64).or(Some(1));
            }
        }
        read_events(&mut host, &mut run);
        if let Some(thread) = run.thread {
            for evidence in inspect(&mut host, &mut run, thread, &request.input, now)? {
                run.exchanges.push(evidence.artifact.value().to_owned());
            }
        }
        // An adapter that never negotiated `terminate` is stopped by dropping
        // it, which kills the process; failing the operation over the tidier
        // exit would throw away the answer it just produced.
        if let Ok(evidence) = host.terminate() {
            note(&mut run, evidence);
        }
        read_events(&mut host, &mut run);

        let value = self.capture(
            &request.actor,
            &serde_json::to_value(&run).map_err(json_error)?,
        )?;
        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: CapabilityAction::DebugLaunch,
                resource: ResourceRef::new("process", &argv[0])
                    .unwrap_or_else(|_| ResourceRef::new("process", "*").expect("a static value")),
            }],
            evidence: Vec::new(),
            state: None,
        })
    }
}

/// Ask where the program is and what it can see there.
fn inspect(
    host: &mut DapHost<StdioDapTransport, Evidence<'_>>,
    run: &mut DebugRun,
    thread: i64,
    input: &Value,
    now_ms: u64,
) -> Result<Vec<DebugEvidence>, OperationError> {
    let mut exchanges = Vec::new();
    let trace = host
        .operate(
            DebugOperation::StackTrace(json!({"threadId": thread, "levels": MAX_FRAMES})),
            now_ms,
        )
        .map_err(dap)?;
    run.frames = array(&trace.body, "stackFrames")
        .into_iter()
        .take(MAX_FRAMES)
        .collect();
    let top = run
        .frames
        .first()
        .and_then(|frame| frame.get("id").cloned());
    exchanges.push(trace);
    let Some(top) = top else {
        return Ok(exchanges);
    };

    let scopes = host
        .operate(DebugOperation::Scopes(json!({"frameId": top})), now_ms)
        .map_err(dap)?;
    let listed = array(&scopes.body, "scopes");
    exchanges.push(scopes);
    for scope in listed {
        let (Some(name), Some(reference)) = (
            scope.get("name").and_then(Value::as_str),
            scope.get("variablesReference").cloned(),
        ) else {
            continue;
        };
        let variables = host
            .operate(
                DebugOperation::Variables(json!({"variablesReference": reference})),
                now_ms,
            )
            .map_err(dap)?;
        run.variables.insert(
            name.to_owned(),
            array(&variables.body, "variables")
                .into_iter()
                .take(MAX_VARIABLES)
                .collect(),
        );
        exchanges.push(variables);
    }

    for expression in input
        .get("expressions")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(Value::as_str)
    {
        let evaluated = host
            .operate(
                DebugOperation::Evaluate(
                    json!({"expression": expression, "frameId": top, "context": "repl"}),
                ),
                now_ms,
            )
            .map_err(dap)?;
        if let Some(result) = evaluated.body.get("result").and_then(Value::as_str) {
            run.evaluated
                .insert(expression.to_owned(), result.to_owned());
        }
        exchanges.push(evaluated);
    }
    Ok(exchanges)
}

/// One array field of an adapter's reply, or nothing.
fn array(body: &Value, key: &str) -> Vec<Value> {
    body.get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Fold whatever the adapter said on its own into the run.
///
/// Only `output` is kept: a debugger run that dropped the program's own
/// printing would be worse than the print statement it replaces. The rest of
/// what an adapter announces -- threads appearing, modules loading -- says
/// nothing about the question that was asked.
fn read_events(host: &mut DapHost<StdioDapTransport, Evidence<'_>>, run: &mut DebugRun) {
    for event in host.events() {
        if event.get("event").and_then(Value::as_str) != Some("output") {
            continue;
        }
        if let Some(text) = event
            .get("body")
            .and_then(|body| body.get("output"))
            .and_then(Value::as_str)
        {
            run.output.push_str(text);
        }
    }
}

/// `[{"path": "src/lib.rs", "lines": [12, 30]}]` as one `setBreakpoints`
/// request per file, with paths made absolute the way an adapter expects.
fn breakpoints(input: &Value, workspace: &std::path::Path) -> Vec<Value> {
    input
        .get("breakpoints")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|breakpoint| {
            let path = breakpoint.get("path").and_then(Value::as_str)?;
            let lines: Vec<Value> = breakpoint
                .get("lines")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_u64)
                .map(|line| json!({"line": line}))
                .collect();
            Some(json!({
                "source": {"path": workspace.join(path).display().to_string()},
                "breakpoints": lines,
            }))
        })
        .collect()
}

fn dap(error: crate::dap::DapError) -> OperationError {
    OperationError::Execution(error.to_string())
}

fn json_error(error: serde_json::Error) -> OperationError {
    OperationError::Execution(error.to_string())
}
