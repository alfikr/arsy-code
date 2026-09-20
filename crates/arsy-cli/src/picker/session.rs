//! The session side of the TUI: loading the workspace's recorded sessions,
//! resuming one, and rebuilding the conversation it had.

#[cfg(feature = "tui")]
use crate::*;
use arsy_kernel::domain::SessionId;
use arsy_kernel::provider::Effort;
use arsy_kernel::secret::{FileCredentialStore, Redactor, SecretError, SecretHandle};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
pub(crate) fn load_workspace_sessions(workspace: &Path) -> Vec<tui::SessionChoice> {
    let Ok(store) = open_store(workspace) else {
        return Vec::new();
    };
    let Ok(summaries) = store.sessions(30) else {
        return Vec::new();
    };
    summaries
        .into_iter()
        .map(|s| {
            let ts = s.last_event_at_ms.or(s.started_at_ms).unwrap_or_default();
            let last_seen = if ts > 0 {
                let now = arsy_kernel::artifact::unix_time_ms();
                let diff_secs = now.saturating_sub(ts) / 1000;
                if diff_secs < 60 {
                    "just now".to_owned()
                } else if diff_secs < 3600 {
                    format!("{}m ago", diff_secs / 60)
                } else if diff_secs < 86400 {
                    format!("{}h ago", diff_secs / 3600)
                } else {
                    format!("{}d ago", diff_secs / 86400)
                }
            } else {
                "recorded".to_owned()
            };
            tui::SessionChoice {
                id: s.session,
                title: s.title,
                events: s.version.0,
                last_seen,
            }
        })
        .collect()
}

#[cfg(feature = "tui")]
/// The conversation a resumed session continues from.
///
/// Built from completed turns only. A turn that failed or was interrupted
/// wrote no completion, so its prompt is not replayed: a question the model
/// never answered, restored as history, reads as something that happened and
/// is worse than a gap.
///
/// Each completed turn contributes the exchange it recorded — prompt,
/// replies, tool calls, tool results — or, for a stream written before
/// transcripts existed, whatever the two ends of it can be reconstructed from.
pub(crate) fn reconstruct_session_conversation(
    workspace: &Path,
    session: SessionId,
) -> (Vec<ModelMessage>, arsy_code::agent::budget::History) {
    let mut history = arsy_code::agent::budget::History::default();
    let Ok(store) = open_store(workspace) else {
        return (Vec::new(), history);
    };
    let Ok(events) = store.read(session, 1, 1000) else {
        return (Vec::new(), history);
    };
    let inline = |event: &arsy_kernel::event::EventEnvelope| {
        let arsy_kernel::event::EventPayload::Inline { data } = &event.payload else {
            return None;
        };
        Some(data.clone())
    };
    let turn_of = |data: &Value| {
        data.get("turn_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    // Two passes, because a transcript is written just before its turn is
    // closed and a turn that never closed must contribute nothing. One pass
    // could not know, at the transcript, whether the completion would come.
    let mut completed = std::collections::HashSet::new();
    let mut prompts: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for event in &events {
        let Some(data) = inline(event) else { continue };
        match event.kind.as_str() {
            "turn.completed" => {
                completed.insert(turn_of(&data));
            }
            "turn.started" => {
                if let Some(prompt) = data.get("prompt").and_then(Value::as_str) {
                    prompts.insert(turn_of(&data), prompt.to_owned());
                }
            }
            _ => {}
        }
    }

    let mut messages = Vec::new();
    let mut transcribed = std::collections::HashSet::new();
    for event in &events {
        let Some(data) = inline(event) else { continue };
        let turn = turn_of(&data);
        if !completed.contains(&turn) {
            continue;
        }
        match event.kind.as_str() {
            "turn.transcript" => {
                let recorded = transcript::restore(data.get("transcript").unwrap_or(&Value::Null));
                if !recorded.is_empty() {
                    transcribed.insert(turn);
                    messages.extend(recorded);
                    // What a later compaction of this prefix would cite: the
                    // event that holds the exchange verbatim.
                    history.citations.push(arsy_kernel::context::EventCitation {
                        id: event.id,
                        sequence: event.sequence,
                    });
                }
            }
            // A stream written before transcripts existed, or one whose
            // transcript did not survive. The question it was asked is what it
            // has, and it is better than nothing.
            "turn.completed" if !transcribed.contains(&turn) => {
                if let Some(prompt) = prompts.remove(&turn) {
                    messages.push(ModelMessage {
                        role: ModelRole::User,
                        content: vec![ModelContent::Text { text: prompt }],
                    });
                }
            }
            _ => {}
        }
    }
    (messages, history)
}

/// The providers configured right now, in the order the configuration lists
/// them. Read fresh each time `/provider` opens, so an edit made outside ARSY
/// is not hidden behind a stale list.
#[cfg(feature = "tui")]
pub(crate) fn configured_providers(invocation: &Invocation) -> Vec<String> {
    let Ok(root) = workspace_root(&invocation.workspace) else {
        return Vec::new();
    };
    let working = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let Ok(config) = load_config(&root, &working, invocation.config.as_deref()) else {
        return Vec::new();
    };
    config.endpoint_ids()
}
