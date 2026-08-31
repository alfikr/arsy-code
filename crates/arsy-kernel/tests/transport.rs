//! stdio transport framing and conformance of live messages to the generated
//! JSON Schema.

use arsy_kernel::{
    domain::{
        AgentId, ApprovalId, ArtifactId, RequestId, ResourceRef, SessionId, StateVersion,
        WorkspaceId,
    },
    protocol::{
        decode_event, event_schema, request_schema, AgentAction, AgentControl, ApprovalResolution,
        ArtifactRead, ClientRequest, Extensions, Initialize, ProtocolEnvelope, ProtocolVersion,
        ServerEvent, SessionCreate, SessionResume, Subscribe, TurnStart, WorkspaceOpen,
    },
    transport::{StdioTransport, MAX_MESSAGE_BYTES},
};
use serde_json::Value;

/// One request per method, so the transport is exercised over the whole
/// client surface rather than the single method the service happens to run.
fn every_request() -> Vec<ProtocolEnvelope<ClientRequest>> {
    let session = SessionId::new();
    vec![
        ClientRequest::Initialize(Initialize {
            protocol_versions: vec![ProtocolVersion::CURRENT],
            capabilities: ["turn.stream".to_owned()].into_iter().collect(),
            extensions: Extensions::new(),
        }),
        ClientRequest::WorkspaceOpen(WorkspaceOpen {
            root: ResourceRef::new("workspace", "/srv/repo").unwrap(),
            extensions: Extensions::new(),
        }),
        ClientRequest::SessionCreate(SessionCreate {
            workspace: WorkspaceId::new(),
            extensions: Extensions::new(),
        }),
        ClientRequest::SessionResume(SessionResume {
            session,
            from_sequence: Some(4),
            extensions: Extensions::new(),
        }),
        ClientRequest::TurnStart(TurnStart {
            session,
            prompt: "hello".to_owned(),
            extensions: Extensions::new(),
        }),
        ClientRequest::ApprovalResolve(ApprovalResolution {
            approval: ApprovalId::new(),
            operation_digest: StateVersion::from_digest([0xab; 32]),
            approved: true,
        }),
        ClientRequest::AgentControl(AgentControl {
            agent: AgentId::new(),
            action: AgentAction::Interrupt,
            extensions: Extensions::new(),
        }),
        ClientRequest::ArtifactRead(ArtifactRead {
            artifact: ArtifactId::new(),
            max_bytes: 4096,
            extensions: Extensions::new(),
        }),
        ClientRequest::Subscribe(Subscribe {
            session,
            from_sequence: None,
            extensions: Extensions::new(),
        }),
    ]
    .into_iter()
    .map(ProtocolEnvelope::new)
    .collect()
}

fn lines(input: &[String]) -> Vec<u8> {
    input.join("\n").into_bytes()
}

fn serve(input: Vec<u8>) -> Vec<ProtocolEnvelope<ServerEvent>> {
    let mut output = Vec::new();
    StdioTransport::new(input.as_slice(), &mut output)
        .serve(|request| {
            let mut events = vec![ServerEvent::Accepted {
                request_id: request.request_id,
                replay: false,
            }];
            if let ClientRequest::Subscribe(subscribe) = &request.payload {
                events.push(ServerEvent::Subscribed {
                    subscription: arsy_kernel::domain::SubscriptionId::new(),
                    next_sequence: subscribe.from_sequence.unwrap_or(1),
                });
            }
            events
        })
        .unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| decode_event(line).unwrap())
        .collect()
}

fn code_of(event: &ProtocolEnvelope<ServerEvent>) -> &str {
    match &event.payload {
        ServerEvent::Failed { code, .. } => code,
        other => panic!("expected failed, got {other:?}"),
    }
}

#[test]
fn stdio_carries_every_request_method_and_its_subscription_events() {
    let requests = every_request();
    let input = lines(
        &requests
            .iter()
            .map(|request| serde_json::to_string(request).unwrap())
            .collect::<Vec<_>>(),
    );

    let events = serve(input);
    // One `accepted` per request, plus the extra `subscribed` for `subscribe`.
    assert_eq!(events.len(), ClientRequest::METHODS.len() + 1);
    let accepted: Vec<RequestId> = events
        .iter()
        .filter_map(|event| match event.payload {
            ServerEvent::Accepted { request_id, .. } => Some(request_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        accepted,
        requests
            .iter()
            .map(|request| request.request_id)
            .collect::<Vec<_>>()
    );
    assert!(matches!(
        events.last().unwrap().payload,
        ServerEvent::Subscribed { .. }
    ));
}

#[test]
fn bounded_lines_and_bad_messages_are_reported_without_ending_the_stream() {
    let request = &every_request()[4];
    let oversized = format!("[{}]", "0,".repeat(MAX_MESSAGE_BYTES / 2));
    assert!(oversized.len() > MAX_MESSAGE_BYTES);
    let unknown_method = serde_json::json!({
        "protocol": { "major": 1, "minor": 0 },
        "request_id": RequestId::new().to_string(),
        "payload": { "method": "self_destruct", "params": {} },
    })
    .to_string();
    let deeply_nested = format!(
        "{{\"payload\":{{\"method\":\"turn_start\",\"params\":{}{}}}}}",
        "[".repeat(256),
        "]".repeat(256)
    );

    let events = serve(lines(&[
        oversized,
        unknown_method,
        deeply_nested,
        String::new(),
        serde_json::to_string(request).unwrap(),
    ]));

    assert_eq!(
        events
            .iter()
            .take(3)
            .map(code_of)
            .collect::<Vec<_>>()
            .as_slice(),
        ["message_too_large", "unknown_variant", "malformed"]
    );
    assert!(matches!(events[3].payload, ServerEvent::Accepted { .. }));
    assert_eq!(events.len(), 4);
}

#[test]
fn live_messages_conform_to_the_generated_schema() {
    let request_schema = request_schema().to_value();
    for request in every_request() {
        check(
            &request_schema,
            &request_schema,
            &serde_json::to_value(&request).unwrap(),
            "request",
        );
    }

    let event_schema = event_schema().to_value();
    let events = [
        ServerEvent::Initialized {
            protocol: ProtocolVersion::CURRENT,
            capabilities: Default::default(),
        },
        ServerEvent::Accepted {
            request_id: RequestId::new(),
            replay: true,
        },
        ServerEvent::Failed {
            request_id: RequestId::new(),
            code: "malformed".to_owned(),
            message: "bad".to_owned(),
        },
    ];
    for event in events {
        check(
            &event_schema,
            &event_schema,
            &serde_json::to_value(ProtocolEnvelope::new(event)).unwrap(),
            "event",
        );
    }
}

#[test]
fn the_generated_schema_lists_exactly_the_declared_methods_and_kinds() {
    assert_eq!(
        tag_values(&request_schema().to_value(), "method"),
        ClientRequest::METHODS.to_vec()
    );
    assert_eq!(
        tag_values(&event_schema().to_value(), "event"),
        ServerEvent::KINDS.to_vec()
    );
}

/// Every `const` a schema pins on `tag`, in schema order.
fn tag_values(schema: &Value, tag: &str) -> Vec<String> {
    let mut found = Vec::new();
    collect_tags(schema, tag, &mut found);
    found
}

fn collect_tags(node: &Value, tag: &str, found: &mut Vec<String>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::String(value)) =
                map.get(tag).and_then(|property| property.get("const"))
            {
                found.push(value.clone());
            }
            for value in map.values() {
                collect_tags(value, tag, found);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_tags(item, tag, found)),
        _ => {}
    }
}

/// Conformance check against the generated schema: follow `$ref`s, pick the
/// `oneOf` branch whose pinned tag matches, and require every property the
/// schema marks required. Catches a Rust type drifting from the wire form.
fn check(root: &Value, schema: &Value, value: &Value, path: &str) {
    if let Some(Value::String(reference)) = schema.get("$ref") {
        let name = reference.rsplit('/').next().unwrap();
        let target = root
            .get("$defs")
            .and_then(|defs| defs.get(name))
            .unwrap_or_else(|| panic!("{path}: schema has no definition for {name}"));
        return check(root, target, value, path);
    }
    if let Some(Value::Array(branches)) = schema.get("oneOf") {
        let branch = branches
            .iter()
            .find(|branch| matches(branch, value))
            .unwrap_or_else(|| panic!("{path}: no schema branch matches {value}"));
        return check(root, branch, value, path);
    }
    let (Some(Value::Array(required)), Some(object)) = (schema.get("required"), value.as_object())
    else {
        return;
    };
    for name in required {
        let name = name.as_str().unwrap();
        assert!(
            object.contains_key(name),
            "{path}: message is missing required property `{name}`"
        );
    }
    for (name, property) in object {
        if let Some(child) = schema.get("properties").and_then(|node| node.get(name)) {
            check(root, child, property, &format!("{path}.{name}"));
        }
    }
}

/// A branch matches when every `const` it pins equals the message's value.
fn matches(branch: &Value, value: &Value) -> bool {
    branch
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| {
            properties.iter().all(|(name, schema)| {
                schema
                    .get("const")
                    .is_none_or(|expected| value.get(name) == Some(expected))
            })
        })
}
