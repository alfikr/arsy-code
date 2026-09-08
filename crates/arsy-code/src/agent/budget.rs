//! What the model is still shown once a turn has run for a while.
//!
//! A tool-using turn grows without bound: twenty rounds of reads and test runs
//! is a transcript larger than the window it has to fit in, and the failure is
//! silent — the provider rejects the request, or worse, quietly drops the front
//! of it, which is where the task was stated.
//!
//! # What gets dropped, and why it is the observations
//!
//! The transcript is not uniformly valuable. The task, the model's own
//! reasoning, and the most recent observations decide the next call; the body
//! of a file read eleven rounds ago does not. So nothing is ever *removed* —
//! removing a tool result would break the call/result pairing every provider
//! requires — and instead the body of an old observation is replaced by a
//! stub naming what it was and where the whole thing still is.
//!
//! ```text
//! [system]  instructions            always kept
//! [user]    the task                always kept
//! [assist]  reasoning + tool calls  always kept — small, and it is the plan
//! [user]    tool results            elided oldest-first, by rank, to fit
//! ```
//!
//! # Why the ranking comes from the kernel
//!
//! [`ContextView::select`] already ranks fragments under a token budget,
//! honours dependencies, and reports what it left out and why. Ranking
//! observations here with a second, private heuristic would mean two
//! selectors whose answers could disagree, and only one of them able to
//! explain itself.

use arsy_kernel::{
    context::{
        Authority, Confidence, ContextCandidate, ContextFragment, ContextScope, ContextView,
        FragmentKind, FragmentSourceKind, Freshness, OmissionReason, RankingWeights,
        SelectionPolicy,
    },
    domain::{ArtifactId, ContextViewId, FragmentId, ResourceRef},
    provider::{ModelContent, ModelMessage},
};
use std::collections::HashMap;

/// Roughly four characters to a token across the families this targets.
///
/// Deliberately an estimate: an exact count needs the provider's tokenizer,
/// which differs per model and is not worth a dependency to decide when to
/// elide a stale file read. The budget is set below the real window to absorb
/// the error.
pub const BYTES_PER_TOKEN: usize = 4;

pub fn estimate_tokens(text: &str) -> u32 {
    u32::try_from(text.len().div_ceil(BYTES_PER_TOKEN)).unwrap_or(u32::MAX)
}

/// How the observations in a transcript are ranked against each other.
///
/// Recency dominates: the observation that decides the next call is almost
/// always the last one. Token cost breaks ties, so between two equally stale
/// results the larger one is elided first and buys more room.
const WEIGHTS: RankingWeights = RankingWeights {
    dependency: 0,
    authority: 0,
    relevance: 1,
    recency: 8,
    confidence: 0,
    novelty: 0,
    token_cost: 2,
};

/// What one round of trimming did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Trimmed {
    /// Observations whose body was replaced by a stub.
    pub elided: usize,
    /// The estimate before and after, in tokens.
    pub before: u32,
    pub after: u32,
}

impl Trimmed {
    pub const fn changed(&self) -> bool {
        self.elided > 0
    }
}

/// Fit `conversation` inside `budget` tokens by eliding stale observations.
///
/// Returns what it did, so a caller can tell the operator rather than letting
/// the transcript shrink invisibly. A conversation already inside the budget is
/// left exactly as it was.
pub fn trim(
    conversation: &mut [ModelMessage],
    budget: u32,
    artifacts: &HashMap<String, ArtifactId>,
) -> Trimmed {
    let before = total_tokens(conversation);
    if before <= budget {
        return Trimmed {
            elided: 0,
            before,
            after: before,
        };
    }

    // Everything that is not an elidable observation is fixed cost. What is
    // left of the budget is what the observations have to fit into.
    let observations = observations(conversation);
    let fixed = before.saturating_sub(
        observations
            .iter()
            .map(|observation| observation.tokens)
            .fold(0u32, u32::saturating_add),
    );
    let remaining = budget.saturating_sub(fixed);

    let fragments: Vec<ContextFragment> = observations
        .iter()
        .filter_map(|observation| {
            ContextFragment::new(
                FragmentId::new(),
                FragmentKind::ToolState,
                ResourceRef::new("tool", observation.id.clone()).ok()?,
                FragmentSourceKind::Tool,
                ContextScope::Global,
                artifacts
                    .get(&observation.id)
                    .copied()
                    .unwrap_or_else(ArtifactId::new),
                observation.tokens,
                // A tool result is the workspace talking, not the operator: it
                // is never trusted context, and the kernel enforces that.
                Authority::Untrusted,
                Confidence::new(10_000).ok()?,
                Freshness::Current,
                Vec::new(),
            )
            .ok()
        })
        .collect();
    let candidates: Vec<ContextCandidate<'_>> = fragments
        .iter()
        .zip(&observations)
        .map(|(fragment, observation)| ContextCandidate {
            fragment,
            residency: "local",
            relevance: 10_000,
            recency: observation.recency,
            novelty: 0,
        })
        .collect();

    let Ok(view) = ContextView::select(
        ContextViewId::new(),
        &candidates,
        &SelectionPolicy {
            scope: ContextScope::Global,
            require_trusted: false,
            allowed_residencies: Vec::new(),
            budget: remaining,
        },
        &WEIGHTS,
    ) else {
        // A selector that cannot answer must not silently keep everything: the
        // request would be rejected. Fall back to eliding the oldest half,
        // which is the same shape of answer with a worse ranking.
        return elide(conversation, &observations, observations.len() / 2, before);
    };

    let dropped: Vec<usize> = view
        .omissions
        .iter()
        .filter(|omission| omission.reason == OmissionReason::TokenBudget)
        .filter_map(|omission| {
            fragments
                .iter()
                .position(|fragment| fragment.id == omission.fragment)
        })
        .collect();
    elide_indexed(conversation, &observations, &dropped, before)
}

/// One tool result in the transcript, and where it lives.
struct Observation {
    /// The tool call id, which is also how the artifact is found again.
    id: String,
    message: usize,
    content: usize,
    tokens: u32,
    /// Higher is newer, in basis points, which is the scale the ranker uses.
    recency: u16,
}

fn observations(conversation: &[ModelMessage]) -> Vec<Observation> {
    let mut found = Vec::new();
    for (message, entry) in conversation.iter().enumerate() {
        for (content, item) in entry.content.iter().enumerate() {
            if let ModelContent::ToolResult {
                id,
                content: body,
                is_error,
            } = item
            {
                // A failed call is small and is exactly what the model needs in
                // order not to repeat it, so it is not worth eliding.
                if *is_error || body.starts_with(ELIDED) {
                    continue;
                }
                found.push(Observation {
                    id: id.clone(),
                    message,
                    content,
                    tokens: estimate_tokens(body),
                    recency: 0,
                });
            }
        }
    }
    let last = found.len().saturating_sub(1).max(1);
    for (position, observation) in found.iter_mut().enumerate() {
        observation.recency = u16::try_from(position * 10_000 / last)
            .unwrap_or(10_000)
            .min(10_000);
    }
    found
}

const ELIDED: &str = "[earlier result elided";

fn elide(
    conversation: &mut [ModelMessage],
    observations: &[Observation],
    count: usize,
    before: u32,
) -> Trimmed {
    let oldest: Vec<usize> = (0..count.min(observations.len())).collect();
    elide_indexed(conversation, observations, &oldest, before)
}

fn elide_indexed(
    conversation: &mut [ModelMessage],
    observations: &[Observation],
    which: &[usize],
    before: u32,
) -> Trimmed {
    let mut elided = 0;
    for index in which {
        let Some(observation) = observations.get(*index) else {
            continue;
        };
        let Some(item) = conversation
            .get_mut(observation.message)
            .and_then(|message| message.content.get_mut(observation.content))
        else {
            continue;
        };
        if let ModelContent::ToolResult { id, content, .. } = item {
            // The stub says what was there and how to get it back, so the model
            // can re-read rather than assume the file was empty.
            *content = format!(
                "{ELIDED} to stay within the context budget; call {id} produced \
                 roughly {} tokens. Read the file or re-run the command if you need it again.]",
                observation.tokens
            );
            elided += 1;
        }
    }
    let after = total_tokens(conversation);
    Trimmed {
        elided,
        before,
        after,
    }
}

fn total_tokens(conversation: &[ModelMessage]) -> u32 {
    conversation
        .iter()
        .flat_map(|message| &message.content)
        .map(|item| match item {
            ModelContent::Text { text } => estimate_tokens(text),
            ModelContent::ToolResult { content, .. } => estimate_tokens(content),
            ModelContent::ToolCall {
                name, arguments, ..
            } => estimate_tokens(name) + estimate_tokens(&arguments.to_string()),
        })
        .fold(0u32, u32::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::provider::ModelRole;
    use serde_json::json;

    fn call(id: &str) -> ModelMessage {
        ModelMessage {
            role: ModelRole::Assistant,
            content: vec![ModelContent::ToolCall {
                id: id.to_owned(),
                name: "fs.read".to_owned(),
                arguments: json!({"path": "big.rs"}),
            }],
        }
    }

    fn result(id: &str, body: &str) -> ModelMessage {
        ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::ToolResult {
                id: id.to_owned(),
                content: body.to_owned(),
                is_error: false,
            }],
        }
    }

    fn transcript(rounds: usize) -> Vec<ModelMessage> {
        let mut conversation = vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "the task".to_owned(),
            }],
        }];
        for round in 0..rounds {
            let id = format!("call-{round}");
            conversation.push(call(&id));
            conversation.push(result(&id, &"x".repeat(4_000)));
        }
        conversation
    }

    fn bodies(conversation: &[ModelMessage]) -> Vec<&str> {
        conversation
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|item| match item {
                ModelContent::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_conversation_inside_its_budget_is_untouched() {
        let mut conversation = transcript(2);
        let original = conversation.clone();

        let trimmed = trim(&mut conversation, 100_000, &HashMap::new());

        assert!(!trimmed.changed());
        assert_eq!(trimmed.before, trimmed.after);
        assert_eq!(conversation, original);
    }

    #[test]
    fn an_overlong_conversation_loses_its_stalest_observations_first() {
        let mut conversation = transcript(6);

        let trimmed = trim(&mut conversation, 1_500, &HashMap::new());

        assert!(trimmed.changed(), "{trimmed:?}");
        assert!(trimmed.after < trimmed.before);
        assert!(
            trimmed.after <= 1_500,
            "the point of the budget is to be met: {trimmed:?}"
        );
        let bodies = bodies(&conversation);
        // The newest observation survives; the oldest does not.
        assert!(!bodies[0].starts_with('x'), "the oldest was elided");
        assert!(
            bodies.last().is_some_and(|body| body.starts_with('x')),
            "the newest observation is what decides the next call"
        );
        // The task is never touched.
        assert!(matches!(
            conversation[0].content.first(),
            Some(ModelContent::Text { text }) if text == "the task"
        ));
    }

    #[test]
    fn every_tool_call_still_has_a_result_after_trimming() {
        let mut conversation = transcript(8);

        trim(&mut conversation, 1_000, &HashMap::new());

        let calls: Vec<&str> = conversation
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|item| match item {
                ModelContent::ToolCall { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        let answered: Vec<&str> = conversation
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|item| match item {
                ModelContent::ToolResult { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls, answered,
            "eliding a body must never remove the result a provider requires"
        );
    }

    #[test]
    fn an_elided_observation_says_what_it_was_and_is_not_elided_twice() {
        let mut conversation = transcript(6);

        trim(&mut conversation, 1_500, &HashMap::new());
        let once = bodies(&conversation)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(once[0].contains("call-0"), "{}", once[0]);
        assert!(once[0].contains("tokens"), "{}", once[0]);

        // Trimming again finds nothing left to take from the stubs.
        trim(&mut conversation, 1_500, &HashMap::new());
        assert_eq!(bodies(&conversation), once);
    }

    #[test]
    fn a_failed_result_is_kept_so_the_model_does_not_repeat_the_call() {
        let mut conversation = transcript(6);
        conversation.push(call("failed"));
        conversation.push(ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::ToolResult {
                id: "failed".to_owned(),
                content: "no line matched the context".to_owned(),
                is_error: true,
            }],
        });

        trim(&mut conversation, 1_000, &HashMap::new());

        assert!(bodies(&conversation).contains(&"no line matched the context"));
    }
}
