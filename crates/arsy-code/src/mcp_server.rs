//! The MCP server adapter: ARSY's operations, offered to an external client.
//!
//! See `docs/24-mcp-acp.md`. This is an edge adapter, so MCP's request types
//! stop here: a `tools/call` becomes an `OperationRequest`, is decided by the
//! same policy engine a local call is decided by, and is dispatched through the
//! same registry.
//!
//! The rule that shapes the module is that a client on the other end of a pipe
//! has no more authority than any other caller. A call policy does not allow is
//! answered with an error naming the decision, and nothing is dispatched — an
//! MCP client cannot become a way around the ceiling.

use crate::operations;
use arsy_kernel::{
    artifact::unix_time_ms,
    capability::{CapabilityGrant, CapabilityRequirement},
    domain::{OperationId, Principal, StateVersion},
    operation::{
        InputSchema, JsonType, OperationContract, OperationKind, OperationRegistry,
        OperationRequest,
    },
    policy::{PolicyDecision, PolicyQuery, RiskContext, RuleSet},
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Revision this adapter speaks. A client offering another one is answered with
/// this, which is what MCP's negotiation expects.
pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// JSON-RPC codes this adapter returns. `-32600`..`-32603` are the standard
/// ones; the policy refusal is in the implementation-defined range.
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const POLICY_REFUSED: i64 = -32020;

pub struct McpServer {
    registry: OperationRegistry,
    rules: RuleSet,
    workspace: PathBuf,
    actor: Principal,
    context: RiskContext,
}

impl McpServer {
    pub fn new(
        registry: OperationRegistry,
        rules: RuleSet,
        workspace: impl AsRef<Path>,
        actor: Principal,
        context: RiskContext,
    ) -> Self {
        Self {
            registry,
            rules,
            workspace: workspace.as_ref().to_path_buf(),
            actor,
            context,
        }
    }

    /// Answer one JSON-RPC message, or `None` when it is a notification.
    ///
    /// A notification has no `id`, and MCP forbids answering one; returning
    /// `None` rather than an error envelope is what keeps a client's stream
    /// correlated.
    pub fn handle(&self, message: &Value) -> Option<Value> {
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str);
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            None => Err(error(INVALID_REQUEST, "message names no method")),
            Some("initialize") => Ok(self.initialize()),
            Some("ping") => Ok(json!({})),
            Some("tools/list") => Ok(self.list_tools()),
            Some("tools/call") => self.call_tool(&params),
            // Everything else is either a notification, which needs no answer,
            // or a method this build does not implement. Advertising only what
            // is implemented is why the second case is an error rather than an
            // empty success.
            Some(other) if other.starts_with("notifications/") => return None,
            Some(other) => Err(error(
                METHOD_NOT_FOUND,
                format!("`{other}` is not implemented"),
            )),
        };
        let id = id?;
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(failure) => json!({"jsonrpc": "2.0", "id": id, "error": failure}),
        })
    }

    fn initialize(&self) -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            // Only what is implemented: no resources, no prompts, no sampling.
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "arsy", "version": crate::VERSION},
        })
    }

    fn list_tools(&self) -> Value {
        let tools: Vec<Value> = self
            .registry
            .kinds()
            .filter_map(|kind| {
                let contract = self.registry.contract(kind)?;
                Some(json!({
                    "name": kind.as_str(),
                    "description": describe(contract),
                    "inputSchema": schema(&contract.input_schema),
                }))
            })
            .collect();
        json!({ "tools": tools })
    }

    fn call_tool(&self, params: &Value) -> Result<Value, Value> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| error(INVALID_PARAMS, "tools/call needs a `name`"))?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let kind = OperationKind::new(name)
            .map_err(|_| error(INVALID_PARAMS, format!("`{name}` is not an operation kind")))?;
        let contract = self
            .registry
            .contract(&kind)
            .ok_or_else(|| error(METHOD_NOT_FOUND, format!("no tool named `{name}`")))?;

        let request = OperationRequest {
            id: OperationId::new(),
            kind,
            actor: self.actor.clone(),
            requirements: operations::requirements(contract, &arguments, &self.workspace),
            input: arguments,
        };
        let digest = request.digest();
        // Reversibility is a property of the operation, not of the transport:
        // policy raises an irreversible call to approval, and taking the answer
        // from the contract is what makes that decision the same one a local
        // caller would get.
        let context = RiskContext {
            reversible: contract.reversible,
            ..self.context
        };
        let grants = self.authorize(&request, digest, context)?;
        let now = unix_time_ms();
        match self.registry.dispatch(&request, &grants, now) {
            Ok(outcome) => Ok(json!({
                "content": [{
                    "type": "text",
                    "text": summarize(&outcome),
                }],
                "isError": false,
                "structuredContent": {
                    "value": outcome.value.as_ref().map(|value| value.value()),
                    "evidence": outcome
                        .evidence
                        .iter()
                        .map(|reference| reference.value())
                        .collect::<Vec<_>>(),
                    "observed_effects": outcome
                        .observed_effects
                        .iter()
                        .map(|effect| json!({
                            "action": effect.action.as_str(),
                            "resource": effect.resource.value(),
                        }))
                        .collect::<Vec<_>>(),
                },
            })),
            // A tool that ran and failed is a tool result, not a protocol
            // error: the client asked a valid question and got an answer.
            Err(failure) => Ok(json!({
                "content": [{"type": "text", "text": failure.to_string()}],
                "isError": true,
            })),
        }
    }

    /// Decide every requirement, and hand back the grants — or refuse.
    ///
    /// Refusal is total: one requirement policy will not allow stops the call,
    /// because dispatching the rest would perform part of an operation nobody
    /// approved.
    fn authorize(
        &self,
        request: &OperationRequest,
        digest: StateVersion,
        context: RiskContext,
    ) -> Result<Vec<CapabilityGrant>, Value> {
        let mut grants = Vec::with_capacity(request.requirements.len());
        for requirement in &request.requirements {
            let query = PolicyQuery {
                actor: request.actor.clone(),
                operation: request.kind.clone(),
                requirement: requirement.clone(),
                operation_digest: digest,
                resource_version: None,
                context,
            };
            match self.rules.evaluate(&query).decision {
                PolicyDecision::Allow(grant) => grants.push(grant),
                PolicyDecision::RequireApproval(approval) => {
                    return Err(refusal(
                        "approval is required and this transport cannot ask for it",
                        requirement,
                        Some(&approval.reason),
                    ))
                }
                PolicyDecision::Deny(reason) => {
                    return Err(refusal(
                        "denied by policy",
                        requirement,
                        Some(&reason.message),
                    ))
                }
            }
        }
        Ok(grants)
    }
}

fn refusal(headline: &str, requirement: &CapabilityRequirement, detail: Option<&str>) -> Value {
    let mut failure = error(
        POLICY_REFUSED,
        format!(
            "{headline}: {} over {}:{}",
            requirement.action,
            requirement.resource.scheme(),
            requirement.resource.value()
        ),
    );
    if let Some(detail) = detail {
        failure["data"] = json!({"reason": detail, "executed": false});
    }
    failure
}

fn error(code: i64, message: impl Into<String>) -> Value {
    json!({"code": code, "message": message.into()})
}

/// A one-line description built from the contract, so it cannot drift from
/// what the operation actually is.
fn describe(contract: &OperationContract) -> String {
    format!(
        "{} — needs {}; {}",
        contract.kind,
        contract
            .actions
            .iter()
            .map(|action| action.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        match contract.idempotency {
            arsy_kernel::operation::Idempotency::Idempotent => "repeatable",
            arsy_kernel::operation::Idempotency::Effectful => "repeats its effect",
        }
    )
}

/// The contract's input schema as JSON Schema, which is what MCP expects.
fn schema(input: &InputSchema) -> Value {
    let properties: serde_json::Map<String, Value> = input
        .required
        .iter()
        .map(|(name, kind)| {
            (
                name.clone(),
                json!({"type": match kind {
                    JsonType::String => "string",
                    JsonType::Number => "number",
                    JsonType::Boolean => "boolean",
                    JsonType::Object => "object",
                    JsonType::Array => "array",
                }}),
            )
        })
        .collect();
    json!({
        "type": "object",
        "properties": properties,
        "required": input.required.keys().collect::<Vec<_>>(),
        "additionalProperties": input.allow_extra,
    })
}

/// The outcome as text. Evidence stays a reference: a tool result is not the
/// place to inline an artifact that the artifact commands already render under
/// redaction.
fn summarize(outcome: &arsy_kernel::operation::OperationOutcome) -> String {
    match &outcome.value {
        Some(value) => format!(
            "completed; result artifact {} and {} evidence artifact(s)",
            value.value(),
            outcome.evidence.len()
        ),
        None => format!(
            "completed with no result and {} evidence artifact(s)",
            outcome.evidence.len()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        artifact::{ArtifactStore, FileArtifactStore},
        capability::{CapabilityAction, PolicySource, ResourcePattern},
        policy::{ActorMatch, PolicyRule, RuleEffect, SandboxAssurance, WorkspaceCleanliness},
    };
    use std::sync::Arc;

    fn allow(action: CapabilityAction, scheme: &str) -> PolicyRule {
        PolicyRule {
            source: PolicySource::User,
            effect: RuleEffect::Allow,
            actor: ActorMatch::Any,
            action,
            pattern: ResourcePattern::new(scheme, "**").unwrap(),
            expires_at_ms: None,
            delegation_depth: 0,
            minimum_assurance: SandboxAssurance::None,
        }
    }

    fn serving(rules: Vec<PolicyRule>) -> (tempfile::TempDir, McpServer) {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::resource::Workspace::open(directory.path()).unwrap();
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(FileArtifactStore::open(directory.path().join("artifacts"), 0).unwrap());
        let registry = operations::registry(
            &workspace,
            artifacts,
            0,
            crate::operations::Reachable::default(),
        )
        .unwrap();
        let server = McpServer::new(
            registry,
            RuleSet::compile(rules),
            directory.path(),
            Principal::User("dev".into()),
            RiskContext {
                reversible: true,
                workspace: WorkspaceCleanliness::Clean,
                sandbox: SandboxAssurance::None,
            },
        );
        (directory, server)
    }

    fn request(id: u64, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    #[test]
    fn negotiation_advertises_only_what_is_implemented() {
        let (_directory, server) = serving(Vec::new());
        let answer = server
            .handle(&request(1, "initialize", json!({})))
            .expect("a request is answered");
        assert_eq!(answer["id"], 1);
        assert_eq!(answer["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(answer["result"]["serverInfo"]["name"], "arsy");
        let capabilities = answer["result"]["capabilities"].as_object().unwrap();
        assert!(capabilities.contains_key("tools"));
        for unimplemented in ["resources", "prompts", "sampling", "elicitation"] {
            assert!(
                !capabilities.contains_key(unimplemented),
                "{unimplemented} is not implemented and must not be advertised"
            );
        }

        // A notification is never answered, and an unknown method is an error
        // rather than a silent success.
        assert!(server
            .handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .is_none());
        let answer = server
            .handle(&request(2, "sampling/createMessage", json!({})))
            .unwrap();
        assert_eq!(answer["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn tools_are_the_registry_and_their_schemas_are_the_contracts() {
        let (_directory, server) = serving(Vec::new());
        let answer = server.handle(&request(1, "tools/list", json!({}))).unwrap();
        let tools = answer["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|tool| tool["name"] == "git.status"));

        let exec = tools
            .iter()
            .find(|tool| tool["name"] == "process.exec")
            .expect("process.exec is registered");
        assert_eq!(exec["inputSchema"]["type"], "object");
        assert_eq!(exec["inputSchema"]["properties"]["argv"]["type"], "array");
        assert_eq!(
            exec["inputSchema"]["properties"]["timeout_ms"]["type"],
            "number"
        );
        assert_eq!(exec["inputSchema"]["additionalProperties"], false);
        assert!(exec["description"]
            .as_str()
            .unwrap()
            .contains("process.exec"));
    }

    #[test]
    fn a_call_policy_refuses_is_never_dispatched() {
        // No rules at all: silence is a denial.
        let (_directory, server) = serving(Vec::new());
        let answer = server
            .handle(&request(
                1,
                "tools/call",
                json!({
                    "name": "process.exec",
                    "arguments": {
                        "argv": ["/bin/echo", "hi"],
                        "timeout_ms": 1_000,
                        "max_output_bytes": 1_024,
                    },
                }),
            ))
            .unwrap();
        assert_eq!(answer["error"]["code"], POLICY_REFUSED);
        assert_eq!(answer["error"]["data"]["executed"], false);
        assert!(answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("process.exec"));

        // A rule that asks for approval is also a refusal here: this transport
        // has nobody to ask.
        let mut ask = allow(CapabilityAction::ProcessExec, "process");
        ask.effect = RuleEffect::RequireApproval;
        let (_directory, server) = serving(vec![ask]);
        let answer = server
            .handle(&request(
                1,
                "tools/call",
                json!({
                    "name": "process.exec",
                    "arguments": {
                        "argv": ["/bin/echo", "hi"],
                        "timeout_ms": 1_000,
                        "max_output_bytes": 1_024,
                    },
                }),
            ))
            .unwrap();
        assert_eq!(answer["error"]["code"], POLICY_REFUSED);
        assert!(answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("approval is required"));

        // An unknown tool is not found, not refused: the two are different
        // facts and a client acts differently on each.
        let (_directory, server) = serving(vec![allow(CapabilityAction::ProcessExec, "process")]);
        let answer = server
            .handle(&request(1, "tools/call", json!({"name": "fs.obliterate"})))
            .unwrap();
        assert_eq!(answer["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn an_allowed_idempotent_call_is_dispatched() {
        let (_directory, server) = serving(vec![allow(CapabilityAction::GitRead, "file")]);
        let answer = server
            .handle(&request(7, "tools/call", json!({"name": "git.status"})))
            .unwrap();
        assert_eq!(answer["id"], 7);
        assert!(
            answer.get("error").is_none(),
            "policy allowed it, so it reached the executor: {}",
            answer["error"]["message"]
        );
        // The temporary workspace is not a repository, so `git status` fails —
        // which is a tool result, not a protocol error.
        assert!(answer["result"]["content"][0]["text"].is_string());

        // A malformed call is the executor's schema error, and nothing ran.
        let answer = server
            .handle(&request(
                8,
                "tools/call",
                json!({"name": "git.status", "arguments": {"unexpected": 1}}),
            ))
            .unwrap();
        assert_eq!(answer["result"]["isError"], true);
    }

    /// An effectful call has nobody to ask over a pipe, so it is refused
    /// however broadly it was allowed. Serving must not be a way to run an
    /// irreversible command unattended.
    #[test]
    fn an_effectful_call_is_refused_even_under_an_allow_rule() {
        let (_directory, server) = serving(vec![allow(CapabilityAction::ProcessExec, "process")]);
        let answer = server
            .handle(&request(
                1,
                "tools/call",
                json!({
                    "name": "process.exec",
                    "arguments": {
                        "argv": ["/bin/echo", "hi"],
                        "timeout_ms": 5_000,
                        "max_output_bytes": 4_096,
                    },
                }),
            ))
            .unwrap();
        assert_eq!(answer["error"]["code"], POLICY_REFUSED);
        assert_eq!(answer["error"]["data"]["executed"], false);
        assert!(answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("approval is required"));
    }
}
