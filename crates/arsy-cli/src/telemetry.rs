//! What a run reports about itself, and where that report may go.
//!
//! The kernel owns the mechanism — a bounded queue, sampling, counters, and a
//! redacted export boundary. This module owns the wiring: which moments in a
//! turn are worth a trace, which numbers are worth a counter, and how the
//! summary reaches the operator.
//!
//! Instrumentation stays out of the turn's decisions. Nothing here can change
//! what the model is asked, which tool runs, or whether a call is allowed; a
//! recorder that fails to build is a diagnostic before the turn starts, and a
//! queue that fills drops events and counts the drops rather than making the
//! turn wait on its own bookkeeping.

use crate::{Diagnostic, Emitter};
use arsy_code::agent::ToolResult;
use arsy_kernel::{
    config::Config,
    domain::{CorrelationId, Principal},
    secret::Redactor,
    telemetry::{
        export_next, HttpExporter, Metric, OpenTelemetryConfig, Telemetry, TelemetryEvent,
        TelemetryKind,
    },
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

/// How long the export may hold a finished run open.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// One run's telemetry: the counters, the queue, and the export decision.
pub struct Recorder {
    telemetry: Telemetry,
    events: Receiver<TelemetryEvent>,
    otel: OpenTelemetryConfig,
    correlation: CorrelationId,
    actor: Principal,
    model_calls: u64,
    tool_calls: u64,
    tool_failures: u64,
}

impl Recorder {
    pub fn new(config: &Config, actor: Principal) -> Result<Self, Diagnostic> {
        let settings = config.telemetry();
        let (telemetry, events) = Telemetry::bounded(settings.capacity, settings.sample_every)
            .map_err(|error| {
                Diagnostic::error(
                    "ARSY-CFG-1001",
                    error.to_string(),
                    "set `telemetry.capacity` and `telemetry.sample_every` to positive integers",
                )
            })?;
        Ok(Self {
            telemetry,
            events,
            otel: settings.otel.clone(),
            correlation: CorrelationId::new(),
            actor,
            model_calls: 0,
            tool_calls: 0,
            tool_failures: 0,
        })
    }

    /// One provider call, however many events its stream carried.
    pub fn model_call(
        &mut self,
        model: &str,
        latency: Duration,
        input_tokens: u64,
        output_tokens: u64,
        retries: u64,
        outcome: &str,
    ) {
        self.model_calls += 1;
        let latency_ms = milliseconds(latency);
        self.telemetry
            .add_metric(Metric::LatencyMilliseconds, latency_ms);
        self.telemetry.add_metric(Metric::InputTokens, input_tokens);
        self.telemetry
            .add_metric(Metric::OutputTokens, output_tokens);
        self.telemetry.add_metric(Metric::Retries, retries);
        self.trace(
            "model.completed",
            [
                // Low cardinality on purpose: a model name and an outcome
                // group across runs, a prompt does not.
                ("model", model.to_owned()),
                ("outcome", outcome.to_owned()),
                ("latency_ms", latency_ms.to_string()),
                ("retries", retries.to_string()),
            ],
        );
    }

    /// One tool call, with the evidence its result was stored as.
    pub fn tool_call(&mut self, result: &ToolResult) {
        self.tool_calls += 1;
        if !result.success {
            self.tool_failures += 1;
        }
        let latency_ms = milliseconds(result.duration);
        self.telemetry
            .add_metric(Metric::ToolLatencyMilliseconds, latency_ms);
        self.trace(
            "tool.completed",
            [
                ("tool", result.tool.clone()),
                (
                    "outcome",
                    if result.success { "ok" } else { "failed" }.to_owned(),
                ),
                ("latency_ms", latency_ms.to_string()),
            ],
        );
    }

    /// Drain the queue, export what the configuration allows, and hand back
    /// the summary a finished run reports.
    ///
    /// `stop` is why the turn ended, which is the question a telemetry record
    /// most often has to answer and the one a pile of counters cannot.
    pub fn finish(self, stop: &str, redactor: &Redactor, emitter: &mut Emitter) -> Value {
        let exported = self.export(redactor, emitter);
        json!({
            "stop": stop,
            "model_calls": self.model_calls,
            "tool_calls": self.tool_calls,
            "tool_failures": self.tool_failures,
            "model_latency_ms": self.telemetry.metric(Metric::LatencyMilliseconds),
            "tool_latency_ms": self.telemetry.metric(Metric::ToolLatencyMilliseconds),
            "input_tokens": self.telemetry.metric(Metric::InputTokens),
            "output_tokens": self.telemetry.metric(Metric::OutputTokens),
            "retries": self.telemetry.metric(Metric::Retries),
            "dropped": self.telemetry.dropped(),
            "exported": exported,
        })
    }

    /// Send what is queued to the configured collector.
    ///
    /// An export failure is reported and never fails the turn: the work is
    /// already done, and losing the record of it is not a reason to lose the
    /// result. The loop is bounded by both the queue and the clock so a
    /// collector that accepts slowly cannot hold the run open.
    fn export(&self, redactor: &Redactor, emitter: &mut Emitter) -> u64 {
        if !self.otel.enabled {
            return 0;
        }
        let mut sink = HttpExporter::new(EXPORT_TIMEOUT);
        let deadline = Instant::now() + EXPORT_TIMEOUT;
        let mut exported = 0;
        while Instant::now() < deadline {
            match export_next(&self.events, &self.otel, redactor, &mut sink) {
                Ok(true) => exported += 1,
                Ok(false) => break,
                Err(error) => {
                    emitter.diagnostic(&Diagnostic::warning(
                        "ARSY-TLM-1000",
                        error.to_string(),
                        "check `telemetry.otel.endpoint`, or set `telemetry.otel.enabled = false`",
                    ));
                    break;
                }
            }
        }
        exported
    }

    fn trace<const N: usize>(&self, name: &str, attributes: [(&str, String); N]) {
        self.telemetry.record(TelemetryEvent {
            kind: TelemetryKind::Trace,
            name: name.to_owned(),
            correlation: self.correlation,
            causation: None,
            actor: self.actor.clone(),
            attributes: attributes
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect::<BTreeMap<_, _>>(),
            evidence: Vec::new(),
            content: None,
        });
    }
}

fn milliseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::config::{Config, Layer};
    use std::path::PathBuf;

    /// A configuration file holding `raw`, written as TOML here and converted
    /// to the `arsy.json` the loader actually reads.
    fn config(raw: &str) -> Config {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(arsy_kernel::config::CONFIG_FILE);
        let json = arsy_kernel::config::json_from_toml(raw, &path).unwrap();
        std::fs::write(&path, json).unwrap();
        Config::load(&[(Layer::User, path)]).unwrap()
    }

    fn recorder(config: &Config) -> Recorder {
        Recorder::new(config, Principal::User("tester".into())).unwrap()
    }

    #[test]
    fn a_finished_run_reports_what_it_spent_and_why_it_stopped() {
        let config = config("schema_version = 1\n");
        let mut recorder = recorder(&config);
        recorder.model_call("m", Duration::from_millis(20), 100, 30, 1, "ok");
        recorder.tool_call(&ToolResult::refused("fs.read", "denied"));

        let mut emitter = Emitter::new(crate::Output::Json);
        let summary = recorder.finish("answered", &Redactor::new(), &mut emitter);

        assert_eq!(summary["stop"], "answered");
        assert_eq!(summary["model_calls"], 1);
        assert_eq!(summary["tool_calls"], 1);
        assert_eq!(summary["tool_failures"], 1);
        assert_eq!(summary["model_latency_ms"], 20);
        assert_eq!(summary["input_tokens"], 100);
        assert_eq!(summary["output_tokens"], 30);
        assert_eq!(summary["retries"], 1);
        // Export is off by default, so nothing left the machine.
        assert_eq!(summary["exported"], 0);
        assert_eq!(summary["dropped"], 0);
    }

    #[test]
    fn sampling_and_queue_depth_come_from_configuration() {
        let config = config("schema_version = 1\n[telemetry]\ncapacity = 1\nsample_every = 100\n");
        let mut recorder = recorder(&config);
        // Every trace after the first is sampled out, so nothing can fill the
        // one-slot queue and no drop is counted.
        for _ in 0..8 {
            recorder.tool_call(&ToolResult::refused("bash", "denied"));
        }

        let mut emitter = Emitter::new(crate::Output::Json);
        let summary = recorder.finish("answered", &Redactor::new(), &mut emitter);

        assert_eq!(summary["tool_calls"], 8, "counters are never sampled");
        assert_eq!(summary["dropped"], 0);
    }

    #[test]
    fn a_repository_may_not_choose_where_run_data_goes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(arsy_kernel::config::CONFIG_FILE);
        let json = arsy_kernel::config::json_from_toml(
            "schema_version = 1\n[telemetry]\nenabled = true\nendpoint = \"https://elsewhere.example/v1\"\n",
            &path,
        )
        .unwrap();
        std::fs::write(&path, json).unwrap();
        let config = Config::load(&[(Layer::Workspace, path.clone())]).unwrap();

        assert!(!config.telemetry().otel.enabled);
        assert!(config.telemetry().otel.endpoint.is_empty());
        assert_eq!(config.diagnostics().len(), 1);

        // The same file in the user layer is honoured.
        let trusted = Config::load(&[(Layer::User, path)]).unwrap();
        assert!(trusted.telemetry().otel.enabled);
    }

    #[test]
    fn an_export_without_https_is_a_configuration_error() {
        let directory = tempfile::tempdir().unwrap();
        let path: PathBuf = directory.path().join(arsy_kernel::config::CONFIG_FILE);
        let json = arsy_kernel::config::json_from_toml(
            "schema_version = 1\n[telemetry]\nendpoint = \"http://collector.example\"\n",
            &path,
        )
        .unwrap();
        std::fs::write(&path, json).unwrap();

        assert!(Config::load(&[(Layer::User, path)]).is_err());
    }
}
