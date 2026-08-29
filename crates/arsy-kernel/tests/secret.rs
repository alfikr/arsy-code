//! Secret broker and redaction pipeline: handles cross boundaries, values do
//! not, and a sink fails closed rather than emitting a credential.

use arsy_kernel::{
    domain::RequestId,
    protocol::ServerEvent,
    provider::anthropic::ApiKey,
    secret::{CredentialStore, SecretBroker, SecretError, SecretHandle},
    transport::StdioTransport,
};
use std::io::Cursor;

const API_KEY: &str = "sk-ant-live-0123456789";

/// Stand-in for the OS keyring: answers for its own store id and nothing else.
struct FakeKeyring {
    id: String,
    entries: Vec<(String, String)>,
}

impl CredentialStore for FakeKeyring {
    fn id(&self) -> &str {
        &self.id
    }

    fn resolve(&self, name: &str) -> Result<String, SecretError> {
        self.entries
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| SecretError::NotFound(SecretHandle::new(&self.id, name).unwrap()))
    }
}

fn keyring() -> Box<dyn CredentialStore> {
    Box::new(FakeKeyring {
        id: "keyring".to_owned(),
        entries: vec![("anthropic".to_owned(), API_KEY.to_owned())],
    })
}

fn handle() -> SecretHandle {
    SecretHandle::new("keyring", "anthropic").unwrap()
}

#[test]
fn a_handle_serializes_without_the_credential_and_round_trips() {
    let handle = handle();
    let encoded = serde_json::to_string(&handle).unwrap();

    assert_eq!(encoded, "\"secret://keyring/anthropic\"");
    assert!(!encoded.contains(API_KEY));
    assert_eq!(
        serde_json::from_str::<SecretHandle>(&encoded).unwrap(),
        handle
    );
    assert_eq!(
        serde_json::from_str::<SecretHandle>("\"keyring/anthropic\"")
            .unwrap_err()
            .to_string(),
        "malformed secret handle: keyring/anthropic"
    );
}

#[test]
fn resolution_needs_a_registered_store_and_never_falls_back() {
    let mut broker = SecretBroker::new();

    assert_eq!(
        broker.resolve(&handle()).unwrap_err(),
        SecretError::UnknownStore("keyring".to_owned()),
        "an unregistered store is an error, not a reason to read plaintext"
    );

    broker.register_store(keyring());
    let secret = broker.resolve(&handle()).unwrap();
    assert_eq!(secret.expose(), API_KEY);
    assert_eq!(
        format!("{secret:?}"),
        "SecretValue(secret://keyring/anthropic)"
    );
    assert_eq!(
        format!("{:?}", ApiKey::from_secret(&secret)),
        "ApiKey(redacted)"
    );

    let missing = SecretHandle::new("keyring", "openai").unwrap();
    assert_eq!(
        broker.resolve(&missing).unwrap_err(),
        SecretError::NotFound(missing)
    );
}

#[test]
fn redaction_is_idempotent_and_refuses_values_it_cannot_hide() {
    let mut broker = SecretBroker::new();
    broker.register_store(keyring());
    broker.resolve(&handle()).unwrap();
    let redactor = broker.redactor();

    let once = redactor
        .sanitize(&format!("authorization: {API_KEY} (retry with {API_KEY})"))
        .unwrap();
    assert_eq!(
        once,
        "authorization: [redacted:secret://keyring/anthropic] \
         (retry with [redacted:secret://keyring/anthropic])"
    );
    assert_eq!(
        redactor.sanitize(&once).unwrap(),
        once,
        "sanitizing an already sanitized payload changes nothing"
    );

    let short = SecretHandle::new("keyring", "pin").unwrap();
    let mut redactor = redactor.clone();
    assert_eq!(
        redactor.register(&short, "1234"),
        Err(SecretError::Unredactable(short)),
        "a value too short to match only itself is refused instead of half-redacted"
    );
}

#[test]
fn the_protocol_adapter_redacts_before_writing() {
    let mut broker = SecretBroker::new();
    broker.register_store(keyring());
    broker.resolve(&handle()).unwrap();

    let mut written = Vec::new();
    let mut transport = StdioTransport::with_redactor(
        Cursor::new(Vec::new()),
        &mut written,
        broker.redactor().clone(),
    );
    transport
        .write_event(ServerEvent::Failed {
            request_id: RequestId::new(),
            code: "provider_auth".to_owned(),
            message: format!("rejected key {API_KEY}"),
        })
        .unwrap();

    let line = String::from_utf8(written).unwrap();
    assert!(
        !line.contains(API_KEY),
        "credential reached the wire: {line}"
    );
    assert!(line.contains("[redacted:secret://keyring/anthropic]"));
}
