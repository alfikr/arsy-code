//! Dependency-free terminal projection over canonical events.

use arsy_kernel::{
    domain::SessionId,
    event::{EventEnvelope, EventPayload},
    policy::{ApprovalRequest, SandboxAssurance},
};
use std::{
    fmt,
    io::{BufRead, Write},
    process::{Command, Stdio},
};

const MAX_TIMELINE_EVENTS: usize = 1_000;
const DEFAULT_WIDTH: usize = 80;
const MIN_WIDTH: usize = 20;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineEntry {
    pub sequence: u64,
    pub name: String,
}

/// Read-only projection: only canonical envelopes can advance its cursor.
pub struct TuiState {
    workspace: String,
    session: SessionId,
    cursor: u64,
    timeline: Vec<TimelineEntry>,
    streaming: Option<String>,
    sandbox_assurance: SandboxAssurance,
    model_route: Option<ModelRoute>,
}

impl TuiState {
    pub fn new(workspace: String, session: SessionId) -> Self {
        Self {
            workspace,
            session,
            cursor: 0,
            timeline: Vec::new(),
            streaming: None,
            sandbox_assurance: SandboxAssurance::None,
            model_route: None,
        }
    }

    pub fn set_sandbox_assurance(&mut self, assurance: SandboxAssurance) {
        self.sandbox_assurance = assurance;
    }

    pub fn set_model_route(&mut self, route: ModelRoute) {
        self.model_route = Some(route);
    }

    pub fn apply(&mut self, event: &EventEnvelope) -> Result<(), TuiError> {
        if event.session != self.session {
            return Err(TuiError::WrongSession);
        }
        let expected = self.cursor.checked_add(1).ok_or(TuiError::CursorOverflow)?;
        if event.sequence != expected {
            return Err(TuiError::Gap {
                expected,
                actual: event.sequence,
            });
        }
        self.cursor = event.sequence;
        self.timeline.push(TimelineEntry {
            sequence: event.sequence,
            name: event.kind.clone(),
        });
        if event.kind == "model.delta" {
            self.streaming = match &event.payload {
                EventPayload::Inline { data } => data
                    .get("text")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned),
                EventPayload::Artifact { .. } => None,
            };
        }
        if self.timeline.len() > MAX_TIMELINE_EVENTS {
            self.timeline.remove(0);
        }
        Ok(())
    }

    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let title = if colour { "\x1b[1mARSY\x1b[0m" } else { "ARSY" };
        let mut lines = vec![
            fit(&format!("{title}  {}", self.workspace), width),
            fit(
                &format!(
                    "session {}  event {}  sandbox {}",
                    self.session, self.cursor, self.sandbox_assurance
                ),
                width,
            ),
        ];
        if let Some(route) = &self.model_route {
            lines.push(fit(&format!("model {route}  mode read-only"), width));
        }
        lines.extend(
            self.timeline
                .iter()
                .rev()
                .take(3)
                .rev()
                .map(|entry| fit(&format!("{:>6}  {}", entry.sequence, entry.name), width)),
        );
        if let Some(text) = &self.streaming {
            lines.push(fit(text, width));
        }
        lines.push(fit("task> ", width));
        lines.join("\n")
    }

    pub fn render_approval(request: &ApprovalRequest, width: usize) -> String {
        [
            "APPROVAL REQUIRED".to_owned(),
            format!("Effect: {}", request.intended_effect()),
            format!("Scope: {}", request.scope()),
            format!("Reversibility: {}", request.reversibility()),
            format!("Reason: {}", request.reason),
            "Choices: [d] deny  [o] approve operation  [r] approve displayed rule".to_owned(),
        ]
        .into_iter()
        .map(|line| fit(&line, width.max(MIN_WIDTH)))
        .collect::<Vec<_>>()
        .join("\n")
            + "\n"
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRoute {
    pub model: String,
}

impl fmt::Display for ModelRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "codex/{}", self.model)
    }
}

pub fn detect_model_route() -> Option<ModelRoute> {
    Command::new("codex")
        .args(["login", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
        .then(|| ModelRoute {
            model: "default".to_owned(),
        })
}

pub fn select_model_route(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    route: &ModelRoute,
) -> std::io::Result<Option<ModelRoute>> {
    writeln!(writer, "Detected logged-in provider: codex")?;
    write!(writer, "model [{}]> ", route.model)?;
    writer.flush()?;
    Ok(read_task(reader)?.map(|model| ModelRoute {
        model: if model.is_empty() {
            route.model.clone()
        } else {
            model
        },
    }))
}

pub fn read_task(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let task = line.trim().to_owned();
    Ok((task != ":quit").then_some(task))
}

pub fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|width| *width > 0)
        .unwrap_or(DEFAULT_WIDTH)
}

fn fit(text: &str, width: usize) -> String {
    let visible = text.replace("\x1b[1m", "").replace("\x1b[0m", "");
    if visible.chars().count() <= width {
        return text.to_owned();
    }
    visible
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>()
        + "…"
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiError {
    WrongSession,
    Gap { expected: u64, actual: u64 },
    CursorOverflow,
}

impl fmt::Display for TuiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongSession => formatter.write_str("event belongs to another session"),
            Self::Gap { expected, actual } => {
                write!(formatter, "event gap: expected {expected}, got {actual}")
            }
            Self::CursorOverflow => formatter.write_str("event cursor overflow"),
        }
    }
}

impl std::error::Error for TuiError {}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        capability::{CapabilityAction, CapabilityRequirement},
        domain::{ApprovalId, CorrelationId, Principal, ResourceRef, StateVersion},
        event::{EventPayload, SchemaVersion},
        operation::OperationKind,
        policy::ApprovalRequest,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn first_frame_stream_resize_no_colour_and_approval_are_complete() {
        let session = SessionId::new();
        let mut state = TuiState::new("/repo".into(), session);
        let started = Instant::now();
        let first = state.render(80, false);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(first.contains("task>"));
        assert!(first.contains("sandbox none"));
        assert!(!first.contains("\x1b["));

        let event = EventEnvelope::new(
            session,
            1,
            Principal::System,
            None,
            CorrelationId::new(),
            SchemaVersion(1),
            "model.delta",
            EventPayload::Inline {
                data: serde_json::json!({"text": "streamed answer"}),
            },
        );
        state.apply(&event).unwrap();
        let narrow = state.render(20, false);
        assert!(narrow.contains("streamed answer"));
        assert!(narrow.lines().all(|line| line.chars().count() <= 20));

        let approval = ApprovalRequest {
            id: ApprovalId::new(),
            actor: Principal::System,
            operation: OperationKind::new("fs.write").unwrap(),
            requirement: CapabilityRequirement {
                action: CapabilityAction::FsWrite,
                resource: ResourceRef::new("file", "/repo/a").unwrap(),
            },
            operation_digest: StateVersion::from_digest([1; 32]),
            reversible: true,
            expires_at_ms: None,
            delegation_depth: 0,
            reason: "workspace rule requires consent".into(),
        };
        let prompt = TuiState::render_approval(&approval, 120);
        for label in ["Effect:", "Scope:", "Reversibility:", "Reason:"] {
            assert!(prompt.contains(label));
        }

        let wrong = EventEnvelope {
            session: SessionId::new(),
            ..event
        };
        assert_eq!(state.apply(&wrong), Err(TuiError::WrongSession));
    }

    #[test]
    fn task_input_stays_open_until_quit_or_eof() {
        let mut input = std::io::Cursor::new(b"inspect the harness\n\n:quit\nignored\n");
        assert_eq!(
            read_task(&mut input).unwrap().as_deref(),
            Some("inspect the harness")
        );
        assert_eq!(read_task(&mut input).unwrap().as_deref(), Some(""));
        assert_eq!(read_task(&mut input).unwrap(), None);

        let mut eof = std::io::Cursor::new(Vec::<u8>::new());
        assert_eq!(read_task(&mut eof).unwrap(), None);
    }

    #[test]
    fn provider_and_model_are_selected_before_tasks() {
        let route = ModelRoute {
            model: "gpt-default".into(),
        };
        let mut input = std::io::Cursor::new(b"gpt-test\n");
        let mut output = Vec::new();
        let selected = select_model_route(&mut input, &mut output, &route).unwrap();
        assert_eq!(
            selected,
            Some(ModelRoute {
                model: "gpt-test".into(),
            })
        );
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("provider: codex"));
    }
}
