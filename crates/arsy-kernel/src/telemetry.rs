//! Bounded local telemetry and an opt-in OpenTelemetry export boundary.

use crate::{
    domain::{CorrelationId, EventId, Principal, ResourceRef, SessionId},
    event::{EventEnvelope, EventStore, StoreError, StreamVersion},
    secret::{Redactor, SecretError},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
};

pub const MAX_EXPORTED_EVENT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryKind {
    Trace,
    Audit,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TelemetryEvent {
    pub kind: TelemetryKind,
    pub name: String,
    pub correlation: CorrelationId,
    pub causation: Option<EventId>,
    pub actor: Principal,
    pub attributes: BTreeMap<String, String>,
    pub evidence: Vec<ResourceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Metric {
    LatencyMilliseconds,
    InputTokens,
    OutputTokens,
    CostMicrounits,
    CacheHits,
    CacheMisses,
    Retries,
}

impl Metric {
    const COUNT: usize = 7;

    const fn index(self) -> usize {
        self as usize
    }
}

pub struct Telemetry {
    sender: SyncSender<TelemetryEvent>,
    sample_every: u64,
    trace_sequence: AtomicU64,
    dropped: AtomicU64,
    metrics: [AtomicU64; Metric::COUNT],
}

impl Telemetry {
    pub fn bounded(
        capacity: usize,
        sample_every: u64,
    ) -> Result<(Self, Receiver<TelemetryEvent>), TelemetryError> {
        if capacity == 0 || sample_every == 0 {
            return Err(TelemetryError::InvalidConfiguration);
        }
        let (sender, receiver) = mpsc::sync_channel(capacity);
        Ok((
            Self {
                sender,
                sample_every,
                trace_sequence: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
                metrics: std::array::from_fn(|_| AtomicU64::new(0)),
            },
            receiver,
        ))
    }

    pub fn record(&self, event: TelemetryEvent) {
        if event.kind == TelemetryKind::Trace
            && !self
                .trace_sequence
                .fetch_add(1, Ordering::Relaxed)
                .is_multiple_of(self.sample_every)
        {
            return;
        }
        if matches!(
            self.sender.try_send(event),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_))
        ) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn add_metric(&self, metric: Metric, value: u64) {
        self.metrics[metric.index()].fetch_add(value, Ordering::Relaxed);
    }

    pub fn metric(&self, metric: Metric) -> u64 {
        self.metrics[metric.index()].load(Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

pub struct TelemetryEventStore<S> {
    inner: S,
    telemetry: Telemetry,
}

impl<S> TelemetryEventStore<S> {
    pub const fn new(inner: S, telemetry: Telemetry) -> Self {
        Self { inner, telemetry }
    }

    pub const fn telemetry(&self) -> &Telemetry {
        &self.telemetry
    }
}

impl<S: EventStore> EventStore for TelemetryEventStore<S> {
    fn current_version(&self, stream: SessionId) -> Result<StreamVersion, StoreError> {
        self.inner.current_version(stream)
    }

    fn append(
        &self,
        stream: SessionId,
        expected: StreamVersion,
        events: Vec<EventEnvelope>,
    ) -> Result<StreamVersion, StoreError> {
        let audit = events
            .iter()
            .map(|event| TelemetryEvent {
                kind: TelemetryKind::Audit,
                name: event.kind.clone(),
                correlation: event.correlation,
                causation: event.causation,
                actor: event.actor.clone(),
                attributes: BTreeMap::new(),
                evidence: Vec::new(),
                content: None,
            })
            .collect::<Vec<_>>();
        let version = self.inner.append(stream, expected, events)?;
        for event in audit {
            self.telemetry.record(event);
        }
        Ok(version)
    }

    fn read(
        &self,
        stream: SessionId,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        self.inner.read(stream, from_sequence, limit)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OpenTelemetryConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub include_content: bool,
}

impl OpenTelemetryConfig {
    pub fn disclosure(&self) -> String {
        format!(
            "OpenTelemetry export to {}; fields: kind, name, correlation, causation, actor, attributes, evidence{}",
            self.endpoint,
            if self.include_content { ", content" } else { "" }
        )
    }
}

pub trait OpenTelemetrySink {
    fn export(&mut self, endpoint: &str, redacted_json: &str) -> Result<(), String>;
}

pub fn export_next(
    receiver: &Receiver<TelemetryEvent>,
    config: &OpenTelemetryConfig,
    redactor: &Redactor,
    sink: &mut dyn OpenTelemetrySink,
) -> Result<bool, TelemetryError> {
    if !config.enabled {
        return Ok(false);
    }
    if !config.endpoint.starts_with("https://") {
        return Err(TelemetryError::InvalidEndpoint);
    }
    let mut event = match receiver.try_recv() {
        Ok(event) => event,
        Err(TryRecvError::Empty | TryRecvError::Disconnected) => return Ok(false),
    };
    if !config.include_content {
        event.content = None;
    }
    let json =
        serde_json::to_string(&event).map_err(|error| TelemetryError::Export(error.to_string()))?;
    let redacted = redactor.sanitize(&json)?;
    if redacted.len() > MAX_EXPORTED_EVENT_BYTES {
        return Err(TelemetryError::ExportTooLarge);
    }
    sink.export(&config.endpoint, &redacted)
        .map_err(TelemetryError::Export)?;
    Ok(true)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TelemetryError {
    InvalidConfiguration,
    InvalidEndpoint,
    ExportTooLarge,
    Export(String),
    Redaction(SecretError),
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration => {
                formatter.write_str("telemetry capacity and sample interval must be non-zero")
            }
            Self::InvalidEndpoint => formatter.write_str("telemetry endpoint must use HTTPS"),
            Self::ExportTooLarge => write!(
                formatter,
                "telemetry event exceeds the {MAX_EXPORTED_EVENT_BYTES}-byte export limit"
            ),
            Self::Export(message) => write!(formatter, "telemetry export failed: {message}"),
            Self::Redaction(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TelemetryError {}

impl From<SecretError> for TelemetryError {
    fn from(value: SecretError) -> Self {
        Self::Redaction(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::SessionId,
        event::{EventPayload, MemoryEventStore, SchemaVersion},
        secret::SecretHandle,
    };
    use serde_json::Value;

    fn event(kind: TelemetryKind) -> TelemetryEvent {
        TelemetryEvent {
            kind,
            name: "model.call".into(),
            correlation: CorrelationId::new(),
            causation: None,
            actor: Principal::System,
            attributes: BTreeMap::from([("outcome".into(), "ok".into())]),
            evidence: Vec::new(),
            content: Some("private content".into()),
        }
    }

    #[derive(Default)]
    struct Sink(Vec<String>);

    impl OpenTelemetrySink for Sink {
        fn export(&mut self, endpoint: &str, json: &str) -> Result<(), String> {
            self.0.push(format!("{endpoint}\n{json}"));
            Ok(())
        }
    }

    #[test]
    fn telemetry_is_bounded_sampled_low_cardinality_and_opt_in() {
        let (telemetry, receiver) = Telemetry::bounded(1, 100).unwrap();
        telemetry.record(event(TelemetryKind::Trace));
        let session = SessionId::new();
        let store = TelemetryEventStore::new(MemoryEventStore::default(), telemetry);
        let canonical = EventEnvelope::new(
            session,
            1,
            Principal::System,
            None,
            CorrelationId::new(),
            SchemaVersion(1),
            "turn.started",
            EventPayload::Inline { data: Value::Null },
        );
        assert_eq!(
            store
                .append(session, StreamVersion(0), vec![canonical])
                .unwrap(),
            StreamVersion(1),
            "a full telemetry queue cannot block the canonical commit"
        );
        assert_eq!(store.telemetry().dropped(), 1);

        store
            .telemetry()
            .add_metric(Metric::LatencyMilliseconds, 12);
        store.telemetry().add_metric(Metric::InputTokens, 7);
        store.telemetry().add_metric(Metric::OutputTokens, 3);
        store.telemetry().add_metric(Metric::CostMicrounits, 9);
        store.telemetry().add_metric(Metric::CacheHits, 1);
        store.telemetry().add_metric(Metric::CacheMisses, 2);
        store.telemetry().add_metric(Metric::Retries, 1);
        assert_eq!(store.telemetry().metric(Metric::Retries), 1);
        drop(receiver.recv().unwrap());

        store.telemetry().record(event(TelemetryKind::Trace));
        store.telemetry().record(event(TelemetryKind::Audit));
        assert_eq!(receiver.recv().unwrap().kind, TelemetryKind::Audit);
        assert!(
            receiver.try_recv().is_err(),
            "sampling applies only to traces"
        );

        let (exporter, export_receiver) = Telemetry::bounded(1, 1).unwrap();
        let mut outbound = event(TelemetryKind::Trace);
        outbound
            .attributes
            .insert("detail".into(), "secret-value".into());
        exporter.record(outbound);
        let disabled = OpenTelemetryConfig {
            endpoint: "https://otel.example/v1/traces".into(),
            ..OpenTelemetryConfig::default()
        };
        assert!(!OpenTelemetryConfig::default().enabled);
        let mut sink = Sink::default();
        assert!(!export_next(&export_receiver, &disabled, &Redactor::new(), &mut sink).unwrap());
        assert!(sink.0.is_empty(), "export is off unless explicitly enabled");

        let handle = SecretHandle::new("test", "token").unwrap();
        let mut redactor = Redactor::new();
        redactor.register(&handle, "secret-value").unwrap();
        let enabled = OpenTelemetryConfig {
            enabled: true,
            ..disabled
        };
        assert_eq!(
            enabled.disclosure(),
            "OpenTelemetry export to https://otel.example/v1/traces; fields: kind, name, correlation, causation, actor, attributes, evidence"
        );
        assert!(export_next(&export_receiver, &enabled, &redactor, &mut sink).unwrap());
        assert!(!sink.0[0].contains("secret-value"));
        assert!(!sink.0[0].contains("private content"));
    }
}
