//! Lifecycle step-start/text/reasoning automaton
//! (TS `protocols/utils/lifecycle.ts`).
//!
//! Every protocol's step function threads this state; event order in the
//! golden replays is defined by this automaton.

use std::collections::BTreeSet;

use crate::schema::events::{LlmEvent, Usage};
use crate::schema::ids::{FinishReason, ProviderMetadata};

/// TS `Lifecycle.State` — which blocks the automaton has already opened.
///
/// TS models `text` / `reasoning` as insertion-ordered `Set`s; the Rust port
/// uses `BTreeSet` for determinism. At most one text and one reasoning block
/// is ever open simultaneously in the supported protocols (each streamed
/// content block is stopped before the next starts), so close ordering cannot
/// diverge in practice.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    pub step_started: bool,
    pub text: BTreeSet<String>,
    pub reasoning: BTreeSet<String>,
}

/// Terminal payload for [`finish`] (TS `Lifecycle.finish` input).
#[derive(Debug, Clone, PartialEq)]
pub struct FinishInput {
    pub reason: FinishReason,
    pub usage: Option<Usage>,
    pub provider_metadata: Option<ProviderMetadata>,
}

/// Create empty lifecycle state for one provider stream.
pub fn initial() -> State {
    State::default()
}

/// Emit `step-start {index: 0}` lazily — only the first call emits.
pub fn step_start(mut state: State, events: &mut Vec<LlmEvent>) -> State {
    if state.step_started {
        return state;
    }
    events.push(LlmEvent::StepStart { index: 0.0 });
    state.step_started = true;
    state
}

/// Emit `text-start` + `text-delta`, opening the block on its first delta.
pub fn text_delta(state: State, events: &mut Vec<LlmEvent>, id: &str, text: &str) -> State {
    let mut state = step_start(state, events);
    if state.text.contains(id) {
        events.push(text_delta_event(id, text));
        return state;
    }
    events.push(LlmEvent::TextStart {
        id: id.to_string(),
        provider_metadata: None,
    });
    events.push(text_delta_event(id, text));
    state.text.insert(id.to_string());
    state
}

/// Emit `reasoning-start`, guarding against duplicate starts.
pub fn reasoning_start(
    state: State,
    events: &mut Vec<LlmEvent>,
    id: &str,
    provider_metadata: Option<ProviderMetadata>,
) -> State {
    if state.reasoning.contains(id) {
        return state;
    }
    let mut state = step_start(state, events);
    events.push(LlmEvent::ReasoningStart {
        id: id.to_string(),
        provider_metadata,
    });
    state.reasoning.insert(id.to_string());
    state
}

/// Emit `reasoning-start` (if needed) followed by `reasoning-delta`.
pub fn reasoning_delta(
    state: State,
    events: &mut Vec<LlmEvent>,
    id: &str,
    text: &str,
    provider_metadata: Option<ProviderMetadata>,
) -> State {
    let state = reasoning_start(state, events, id, provider_metadata);
    events.push(LlmEvent::ReasoningDelta {
        id: id.to_string(),
        text: text.to_string(),
        provider_metadata: None,
    });
    state
}

/// Emit `reasoning-end` for an open block; a no-op for unknown ids.
pub fn reasoning_end(
    state: State,
    events: &mut Vec<LlmEvent>,
    id: &str,
    provider_metadata: Option<ProviderMetadata>,
) -> State {
    if !state.reasoning.contains(id) {
        return state;
    }
    let mut state = step_start(state, events);
    events.push(LlmEvent::ReasoningEnd {
        id: id.to_string(),
        provider_metadata,
    });
    state.reasoning.remove(id);
    state
}

/// Emit `text-end` for an open block; a no-op for unknown ids.
pub fn text_end(
    state: State,
    events: &mut Vec<LlmEvent>,
    id: &str,
    provider_metadata: Option<ProviderMetadata>,
) -> State {
    if !state.text.contains(id) {
        return state;
    }
    let mut state = step_start(state, events);
    events.push(LlmEvent::TextEnd {
        id: id.to_string(),
        provider_metadata,
    });
    state.text.remove(id);
    state
}

fn close_open_blocks(mut state: State, events: &mut Vec<LlmEvent>) -> State {
    for id in state.reasoning.iter() {
        events.push(LlmEvent::ReasoningEnd {
            id: id.clone(),
            provider_metadata: None,
        });
    }
    for id in state.text.iter() {
        events.push(LlmEvent::TextEnd {
            id: id.clone(),
            provider_metadata: None,
        });
    }
    state.reasoning.clear();
    state.text.clear();
    state
}

/// Close any open text/reasoning blocks, then emit `step-finish` + `finish`
/// and reset `step_started`.
pub fn finish(state: State, events: &mut Vec<LlmEvent>, input: FinishInput) -> State {
    let mut state = close_open_blocks(step_start(state, events), events);
    events.push(LlmEvent::StepFinish {
        index: 0.0,
        reason: input.reason,
        usage: input.usage.clone(),
        provider_metadata: input.provider_metadata.clone(),
    });
    events.push(LlmEvent::Finish {
        reason: input.reason,
        usage: input.usage,
        provider_metadata: input.provider_metadata,
    });
    state.step_started = false;
    state
}

fn text_delta_event(id: &str, text: &str) -> LlmEvent {
    LlmEvent::TextDelta {
        id: id.to_string(),
        text: text.to_string(),
        provider_metadata: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_step_start_exactly_once_across_two_text_deltas() {
        let mut events = Vec::new();
        let state = text_delta(initial(), &mut events, "text-0", "Hel");
        text_delta(state, &mut events, "text-0", "lo!");
        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart { index: 0.0 },
                LlmEvent::TextStart {
                    id: "text-0".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::TextDelta {
                    id: "text-0".to_string(),
                    text: "Hel".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::TextDelta {
                    id: "text-0".to_string(),
                    text: "lo!".to_string(),
                    provider_metadata: None,
                },
            ],
        );
    }

    #[test]
    fn text_start_pairs_the_first_delta_only() {
        let mut events = Vec::new();
        let state = text_delta(initial(), &mut events, "text-1", "a");
        let state = text_delta(state, &mut events, "text-0", "b");
        let state = text_delta(state, &mut events, "text-1", "c");
        text_end(state, &mut events, "text-0", None);
        let step_starts = events
            .iter()
            .filter(|event| matches!(event, LlmEvent::StepStart { .. }))
            .count();
        assert_eq!(step_starts, 1);
        let text_starts = events
            .iter()
            .filter(|event| matches!(event, LlmEvent::TextStart { .. }))
            .count();
        assert_eq!(text_starts, 2);
        let text_ends = events
            .iter()
            .filter(|event| matches!(event, LlmEvent::TextEnd { .. }))
            .count();
        assert_eq!(text_ends, 1);
    }

    #[test]
    fn text_end_is_a_no_op_for_unknown_ids() {
        let mut events = Vec::new();
        let state = text_end(initial(), &mut events, "text-0", None);
        assert_eq!(state, initial());
        assert!(events.is_empty());
    }

    #[test]
    fn reasoning_start_guards_duplicates() {
        let mut events = Vec::new();
        let state = reasoning_start(initial(), &mut events, "reasoning-0", None);
        let state = reasoning_start(state, &mut events, "reasoning-0", None);
        let reasoning_starts = events
            .iter()
            .filter(|event| matches!(event, LlmEvent::ReasoningStart { .. }))
            .count();
        assert_eq!(reasoning_starts, 1);

        // A repeated reasoning delta still emits its delta event.
        let mut delta_events = Vec::new();
        let state = reasoning_delta(state, &mut delta_events, "reasoning-0", "thinking", None);
        assert_eq!(
            delta_events,
            vec![LlmEvent::ReasoningDelta {
                id: "reasoning-0".to_string(),
                text: "thinking".to_string(),
                provider_metadata: None,
            }],
        );

        reasoning_end(state, &mut delta_events, "reasoning-0", None);
        assert_eq!(
            delta_events[1],
            LlmEvent::ReasoningEnd {
                id: "reasoning-0".to_string(),
                provider_metadata: None,
            },
        );
    }

    #[test]
    fn finish_closes_open_blocks_then_emits_step_finish_and_finish() {
        let mut events = Vec::new();
        let state = text_delta(initial(), &mut events, "text-0", "partial");
        let state = reasoning_start(state, &mut events, "reasoning-0", None);
        let state = finish(
            state,
            &mut events,
            FinishInput {
                reason: FinishReason::Stop,
                usage: None,
                provider_metadata: None,
            },
        );
        finish(
            state,
            &mut events,
            FinishInput {
                reason: FinishReason::Stop,
                usage: None,
                provider_metadata: None,
            },
        );

        let tail = &events[events.len() - 7..];
        assert_eq!(
            tail.to_vec(),
            vec![
                LlmEvent::ReasoningEnd {
                    id: "reasoning-0".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::TextEnd {
                    id: "text-0".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::StepFinish {
                    index: 0.0,
                    reason: FinishReason::Stop,
                    usage: None,
                    provider_metadata: None,
                },
                LlmEvent::Finish {
                    reason: FinishReason::Stop,
                    usage: None,
                    provider_metadata: None,
                },
                // Second finish re-emits step-start (step_started was reset)
                // but no block-close events (sets were cleared).
                LlmEvent::StepStart { index: 0.0 },
                LlmEvent::StepFinish {
                    index: 0.0,
                    reason: FinishReason::Stop,
                    usage: None,
                    provider_metadata: None,
                },
                LlmEvent::Finish {
                    reason: FinishReason::Stop,
                    usage: None,
                    provider_metadata: None,
                },
            ],
        );
    }
}
