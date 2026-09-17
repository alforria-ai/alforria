//! The semantic API contract of one model server family.
//!
//! Port of `route/protocol.ts` (M2.6). A [`Protocol`] owns the parts of a
//! route that are intrinsic to "what does this API look like": how a common
//! [`LlmRequest`] becomes a provider-native body, and how the streaming
//! response decodes back into common [`LlmEvent`]s.
//!
//! A `Protocol` is **not** a deployment. It does not know which URL, which
//! headers, or which auth scheme to use. Those are deployment concerns owned
//! by the route alongside the chosen [`crate::route::endpoint::Endpoint`],
//! [`crate::route::auth::Auth`], and [`crate::route::framing::Framing`].
//!
//! Differences from the TS reference (spec §2.6): the Effect `Schema` codec
//! machinery does not port. `body.schema` validation is serde serialization
//! of the typed body (`lower_body` emits `serde_json::Value` directly), and
//! the `stream.event` decode is collapsed into [`Protocol::decode_frame`],
//! which turns one framed `serde_json::Value` into this protocol's event
//! (or `None` to drop the frame). Implementers should prefer concrete typed
//! `Event`/`State` internally and erase at the handle boundary.

#![allow(clippy::result_large_err)]

use crate::schema::errors::LlmError;
use crate::schema::events::LlmEvent;
use crate::schema::messages::LlmRequest;

/// One model server family: request lowering plus the streaming state
/// machine that translates provider events into [`LlmEvent`]s.
pub trait Protocol {
    /// Stable id for the wire protocol implementation.
    const ID: &'static str;

    /// Accumulator threaded through the streaming state machine.
    type State;

    /// Lower a common LLMRequest into the provider-native JSON body.
    fn lower_body(&self, request: &LlmRequest) -> Result<serde_json::Value, LlmError>;

    /// Decode one transport frame into this protocol's event, or drop it (None).
    fn decode_frame(
        &self,
        frame: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, LlmError>;

    /// Initial parser state. Called once per response with the resolved request.
    fn initial(&self, request: &LlmRequest) -> Self::State;

    /// Translate one event into emitted [`LlmEvent`]s plus the next state.
    fn step(
        &self,
        state: Self::State,
        event: &serde_json::Value,
    ) -> Result<(Self::State, Vec<LlmEvent>), LlmError>;

    /// Optional request-completion signal for transports that do not end
    /// naturally (openai-responses: `response.completed/incomplete/failed`).
    fn terminal(&self, _event: &serde_json::Value) -> bool {
        false
    }

    /// Optional flush emitted when the framed stream ends.
    fn on_halt(&self, _state: Self::State) -> Vec<LlmEvent> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct IdleProtocol;

    impl Protocol for IdleProtocol {
        const ID: &'static str = "idle";
        type State = ();

        fn lower_body(&self, _request: &LlmRequest) -> Result<serde_json::Value, LlmError> {
            Ok(serde_json::json!({}))
        }

        fn decode_frame(
            &self,
            _frame: &serde_json::Value,
        ) -> Result<Option<serde_json::Value>, LlmError> {
            Ok(None)
        }

        fn initial(&self, _request: &LlmRequest) -> Self::State {}

        fn step(
            &self,
            state: Self::State,
            _event: &serde_json::Value,
        ) -> Result<(Self::State, Vec<LlmEvent>), LlmError> {
            Ok((state, Vec::new()))
        }
    }

    #[test]
    fn default_terminal_and_on_halt() {
        let protocol = IdleProtocol;
        assert_eq!(IdleProtocol::ID, "idle");
        assert!(!protocol.terminal(&serde_json::json!({})));
        assert!(protocol.on_halt(()).is_empty());
    }
}
