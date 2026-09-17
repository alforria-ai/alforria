//! Route for non-OpenAI providers that expose an OpenAI Chat-compatible
//! `/chat/completions` endpoint (TS `protocols/openai-compatible-chat.ts`).
//!
//! Reuses [`crate::protocols::openai_chat`] end-to-end and overrides only the
//! route id so providers can be resolved per-family without colliding with
//! native OpenAI. Providers configure the route endpoint (baseURL) before
//! model selection, so the route ships no default endpoint.

#![allow(clippy::arc_with_non_send_sync)]

use crate::protocols::openai_chat;
use crate::route::auth::Auth;
use crate::route::client::{RouteDefaults, RouteHandle};
use crate::route::endpoint::Endpoint;
use crate::route::framing::Framing;

pub const ADAPTER: &str = "openai-compatible-chat";

/// Route constants for the openai-compatible deployment: the openai-chat
/// protocol behind the `openai-compatible-chat` route id, with no default
/// endpoint.
pub fn route_handle() -> RouteHandle {
    RouteHandle {
        id: ADAPTER.to_string(),
        protocol_id: openai_chat::ADAPTER.to_string(),
        endpoint: Endpoint::path(openai_chat::PATH),
        auth: Auth::none(),
        framing: Framing::Sse,
        defaults: RouteDefaults::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_handle_reuses_the_openai_chat_protocol() {
        let handle = route_handle();
        assert_eq!(handle.id, "openai-compatible-chat");
        assert_eq!(handle.protocol_id, "openai-chat");
        // No default endpoint: providers configure the baseURL.
        assert_eq!(handle.endpoint.base_url, None);
        assert!(matches!(
            handle.endpoint.path,
            crate::route::endpoint::EndpointPart::Path(ref path)
                if path == "/chat/completions"
        ));
    }
}
