//! A provider that replays a recorded conversation instead of calling one.
//!
//! # What this is for
//!
//! Every gate phrased as "beats the baseline" needs the same task run twice
//! under different configurations, many times, and compared. With a live model
//! that is expensive, slow, and — because the model is sampling — not a
//! comparison of the two configurations at all: it is a comparison of two
//! samples. Replaying a fixed script holds the model still, so what the
//! measurement moves is the harness.
//!
//! It is also how a failing run becomes a test. A script recorded from a real
//! session reproduces that session exactly, on a machine with no credential
//! and no network.
//!
//! # What it is not
//!
//! Not a model. A replayed run measures what the harness does with a given set
//! of model outputs — which tools it dispatches, what it spends, what it
//! refuses — and says nothing about whether the model was any good. Reading a
//! replay's success rate as a model's is the one mistake this module makes
//! easy, so its descriptor says `replay` and the script is a file an operator
//! chose.

use super::{
    CanonicalModelRequest, ModelEvent, ModelEventStream, ModelProvider, ProviderDescriptor,
    ProviderError, StopReason,
};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Largest script this will read. A script is a conversation, not a corpus.
pub const MAX_SCRIPT_BYTES: u64 = 8 * 1024 * 1024;

/// One recorded reply: what the model said, and what it asked to run.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    /// What the model answered, streamed as one chunk per entry so a test can
    /// exercise a client's own reassembly.
    #[serde(default)]
    text: Vec<String>,
    #[serde(default)]
    tool_calls: Vec<ScriptedCall>,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptedCall {
    id: String,
    name: String,
    /// The call's arguments, as the model would have produced them.
    arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Script {
    replies: Vec<Reply>,
}

#[derive(Debug)]
pub struct ReplayProvider {
    descriptor: ProviderDescriptor,
    replies: Vec<Reply>,
    /// How many requests have been answered. A replay is a sequence, so the
    /// position is the whole of its state.
    served: Mutex<usize>,
    path: PathBuf,
}

impl ReplayProvider {
    /// Read a script from disk.
    pub fn open(path: impl AsRef<Path>, id: &str) -> Result<Self, ProviderError> {
        let path = path.as_ref().to_path_buf();
        let length = std::fs::metadata(&path)
            .map_err(|error| ProviderError::InvalidRequest(format!("{}: {error}", path.display())))?
            .len();
        if length > MAX_SCRIPT_BYTES {
            return Err(ProviderError::InvalidRequest(format!(
                "{} exceeds {MAX_SCRIPT_BYTES} bytes",
                path.display()
            )));
        }
        let bytes = std::fs::read(&path).map_err(|error| {
            ProviderError::InvalidRequest(format!("{}: {error}", path.display()))
        })?;
        let script: Script = serde_json::from_slice(&bytes).map_err(|error| {
            ProviderError::InvalidRequest(format!(
                "{} is not a replay script: {error}",
                path.display()
            ))
        })?;
        if script.replies.is_empty() {
            return Err(ProviderError::InvalidRequest(format!(
                "{} scripts no replies",
                path.display()
            )));
        }
        Ok(Self {
            descriptor: ProviderDescriptor {
                id: id.to_owned(),
                // Retrying a replay would replay the next line, which is not
                // the same request. A script that has run out is an error, not
                // something to try again.
                max_retries: 0,
            },
            replies: script.replies,
            served: Mutex::new(0),
            path,
        })
    }

    /// A `file:` URL or a bare path, as configuration writes one.
    pub fn from_base_url(base_url: &str, id: &str) -> Result<Self, ProviderError> {
        Self::open(
            base_url
                .strip_prefix("file://")
                .unwrap_or_else(|| base_url.strip_prefix("file:").unwrap_or(base_url)),
            id,
        )
    }
}

impl ModelProvider for ReplayProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn stream(&self, _request: &CanonicalModelRequest) -> Result<ModelEventStream, ProviderError> {
        let mut served = self
            .served
            .lock()
            .map_err(|_| ProviderError::Transport("replay position is poisoned".to_owned()))?;
        let Some(reply) = self.replies.get(*served) else {
            // Running past the end means the harness took a path the script
            // did not anticipate. Saying so names the real problem; answering
            // with silence would look like the model gave up.
            return Err(ProviderError::InvalidRequest(format!(
                "{} scripts {} replies and this is request {}",
                self.path.display(),
                self.replies.len(),
                *served + 1
            )));
        };
        *served += 1;

        let mut events: Vec<Result<ModelEvent, ProviderError>> = reply
            .text
            .iter()
            .map(|text| Ok(ModelEvent::TextDelta { text: text.clone() }))
            .collect();
        for (index, call) in reply.tool_calls.iter().enumerate() {
            events.push(Ok(ModelEvent::ToolCallCompleted {
                index,
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            }));
        }
        if reply.input_tokens > 0 || reply.output_tokens > 0 {
            events.push(Ok(ModelEvent::Usage {
                input_tokens: reply.input_tokens,
                output_tokens: reply.output_tokens,
            }));
        }
        events.push(Ok(ModelEvent::Completed {
            stop: if reply.tool_calls.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolUse
            },
        }));
        Ok(Box::new(events.into_iter()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{protocol::IdempotencyKey, provider::ModelKey};

    fn script(body: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("script.json");
        std::fs::write(&path, body).unwrap();
        (directory, path)
    }

    fn request() -> CanonicalModelRequest {
        CanonicalModelRequest {
            model: ModelKey {
                provider: "replay".into(),
                model: "recorded".into(),
            },
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: 128,
            effort: None,
            idempotency_key: IdempotencyKey::new("one").unwrap(),
        }
    }

    fn collect(stream: ModelEventStream) -> Vec<ModelEvent> {
        stream.map(Result::unwrap).collect()
    }

    #[test]
    fn a_script_is_replayed_one_reply_per_request_in_order() {
        let (_directory, path) = script(
            r#"{"replies": [
                {"text": ["let me "], "tool_calls": [
                    {"id": "c1", "name": "fs.read", "arguments": {"path": "a.rs"}}
                ], "input_tokens": 10, "output_tokens": 2},
                {"text": ["it says hello."], "input_tokens": 20, "output_tokens": 4}
            ]}"#,
        );
        let provider = ReplayProvider::open(&path, "replay").unwrap();

        let first = collect(provider.stream(&request()).unwrap());
        assert!(matches!(first[0], ModelEvent::TextDelta { .. }));
        assert!(matches!(
            first[1],
            ModelEvent::ToolCallCompleted { ref name, .. } if name == "fs.read"
        ));
        assert!(matches!(
            first[2],
            ModelEvent::Usage {
                input_tokens: 10,
                output_tokens: 2
            }
        ));
        // A reply that asked for a tool did not end the turn.
        assert!(matches!(
            first[3],
            ModelEvent::Completed {
                stop: StopReason::ToolUse
            }
        ));

        let second = collect(provider.stream(&request()).unwrap());
        assert!(matches!(
            second.last(),
            Some(ModelEvent::Completed {
                stop: StopReason::EndTurn
            })
        ));

        // Past the end is an error that names the script, because the harness
        // took a path the recording did not.
        // `ModelEventStream` is a boxed iterator and not `Debug`, so the
        // error is matched rather than unwrapped out of the Ok side.
        let Err(error) = provider.stream(&request()) else {
            panic!("a script that has run out cannot answer");
        };
        assert!(format!("{error}").contains("request 3"), "{error}");
    }

    #[test]
    fn a_replay_is_never_retried() {
        let (_directory, path) = script(r#"{"replies": [{"text": ["one"]}]}"#);
        let provider = ReplayProvider::open(&path, "replay").unwrap();
        assert_eq!(
            provider.descriptor().max_retries,
            0,
            "retrying would replay a different line as though it were the same request"
        );
    }

    #[test]
    fn a_script_that_is_not_one_is_refused_by_name() {
        let (_directory, path) = script(r#"{"replies": []}"#);
        let empty = ReplayProvider::open(&path, "replay").unwrap_err();
        assert!(format!("{empty}").contains("no replies"), "{empty}");

        let (_directory, path) = script(r#"{"turns": []}"#);
        let wrong = ReplayProvider::open(&path, "replay").unwrap_err();
        assert!(format!("{wrong}").contains("replay script"), "{wrong}");

        let missing = ReplayProvider::open("/no/such/script.json", "replay").unwrap_err();
        assert!(format!("{missing}").contains("script.json"), "{missing}");
    }

    #[test]
    fn a_path_is_taken_from_a_file_url_or_written_plainly() {
        let (_directory, path) = script(r#"{"replies": [{"text": ["one"]}]}"#);
        assert!(ReplayProvider::from_base_url(path.to_str().unwrap(), "replay").is_ok());
        assert!(
            ReplayProvider::from_base_url(&format!("file://{}", path.display()), "replay").is_ok()
        );
    }
}
