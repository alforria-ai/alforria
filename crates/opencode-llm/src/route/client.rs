//! Route & client pipeline (TS `route/client.ts`): a `Route` binds a
//! protocol + endpoint + auth + framing + static defaults behind a
//! type-erased `RouteHandle`, and drives the
//! compile → prepare → stream → generate pipeline.

use std::sync::Arc;

use crate::cache_policy::apply_cache_policy;
use crate::route::auth::{Auth, AuthInput, Headers};
use crate::route::endpoint::{render, Endpoint, EndpointInput};
use crate::route::executor::{PreparedRequest, RequestExecutor};
use crate::route::framing::Framing;
use crate::route::protocol::Protocol;
use crate::schema::errors::LlmError;
use crate::schema::events::{LlmEvent, LlmResponse};
use crate::schema::messages::LlmRequest;
use crate::schema::options::{
    merge_generation_options, merge_http_options, merge_provider_options, GenerationOptions,
    HttpOptions, ModelLimits, ProviderOptions,
};

/// Static per-route defaults merged under model defaults and request options.
#[derive(Clone, Default)]
pub struct RouteDefaults {
    pub headers: Headers,
    pub limits: Option<ModelLimits>,
    pub generation: Option<GenerationOptions>,
    pub provider_options: Option<ProviderOptions>,
    pub http: Option<HttpOptions>,
}

/// Deployment concern (protocol + endpoint + auth + transport) erased behind
/// an `Arc` in `Model`/`ModelRef`.
pub struct RouteHandle {
    /// Route id ("anthropic-messages", "openai-compatible-chat", …).
    pub id: String,
    pub protocol_id: String,
    pub endpoint: Endpoint,
    pub auth: Auth,
    pub framing: Framing,
    pub defaults: RouteDefaults,
}

impl RouteHandle {
    /// A stand-in handle used when deserializing requests built in memory
    /// (no live route exists yet).
    pub fn empty() -> RouteHandle {
        RouteHandle {
            id: String::new(),
            protocol_id: String::new(),
            endpoint: Endpoint::path("/"),
            auth: Auth::none(),
            framing: Framing::Sse,
            defaults: RouteDefaults::default(),
        }
    }
}

impl Default for RouteHandle {
    fn default() -> Self {
        RouteHandle::empty()
    }
}

impl std::fmt::Debug for RouteHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteHandle")
            .field("id", &self.id)
            .field("protocol_id", &self.protocol_id)
            .field("endpoint", &self.endpoint)
            .field("framing", &self.framing)
            .finish_non_exhaustive()
    }
}

/// A route binds a concrete protocol instance to its deployment handle and
/// drives request execution.
pub struct Route<P> {
    pub handle: Arc<RouteHandle>,
    pub protocol: Arc<P>,
    pub executor: RequestExecutor,
}

impl<P: Protocol> Route<P> {
    pub fn new(handle: RouteHandle, protocol: P) -> Self {
        Route {
            #[allow(clippy::arc_with_non_send_sync)]
            handle: Arc::new(handle),
            protocol: Arc::new(protocol),
            executor: RequestExecutor::new(),
        }
    }

    /// Resolve request options by merging route defaults ← model defaults ←
    /// request (last wins).
    pub fn resolve_request_options(&self, request: &LlmRequest) -> LlmRequest {
        let model = &request.model;
        let route = &self.handle.defaults;
        LlmRequest {
            generation: merge_generation_options(&[
                route.generation.as_ref(),
                model.defaults.as_ref().and_then(|d| d.generation.as_ref()),
                request.generation.as_ref(),
            ]),
            provider_options: merge_provider_options(&[
                route.provider_options.as_ref(),
                model
                    .defaults
                    .as_ref()
                    .and_then(|d| d.provider_options.as_ref()),
                request.provider_options.as_ref(),
            ]),
            http: merge_http_options(&[
                route.http.as_ref(),
                model.defaults.as_ref().and_then(|d| d.http.as_ref()),
                request.http.as_ref(),
            ]),
            ..request.clone()
        }
    }

    /// Compile a request into a prepared HTTP request: merge options,
    /// apply the cache policy, lower the body, and prepare transport.
    #[allow(clippy::result_large_err)]
    pub fn compile(&self, request: &LlmRequest) -> Result<PreparedRequest, LlmError> {
        let resolved = self.resolve_request_options(request);
        let resolved = apply_cache_policy(&self.handle.id, &resolved);
        let body = self.protocol.lower_body(&resolved)?;
        let body_text = serde_json::to_string(&body)
            .map_err(|e| LlmError::invalid(format!("Route.compile: {e}")))?;

        let input = EndpointInput {
            request: &resolved,
            body: &body,
        };
        let url = render(&self.handle.endpoint, &input)?.as_str().to_string();

        let mut headers = self.handle.defaults.headers.clone();
        let auth_headers = self.handle.auth.apply(&AuthInput {
            request: &resolved,
            method: "POST",
            url: &url,
            body: &body_text,
            headers: &headers,
        })?;
        for (name, value) in auth_headers {
            upsert_header(&mut headers, name, value);
        }
        upsert_header(
            &mut headers,
            "content-type".to_string(),
            "application/json".to_string(),
        );

        Ok(PreparedRequest {
            method: "POST".to_string(),
            url,
            headers,
            body: body_text,
        })
    }

    /// Stream one request: POST the prepared request and turn the response
    /// bytes into the protocol-decoded [`LlmEvent`] stream.
    pub async fn stream(
        &self,
        request: &LlmRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError>
    where
        P: Send + Sync + 'static,
        P::State: Send + 'static,
    {
        Self::stream_with_halt(self, request, tokio_util::sync::CancellationToken::new()).await
    }

    /// `Stream.mapAccumEffect`'s `onHalt` (route/client.ts:287-291): the
    /// parser state flushes when the stream halts — which in Effect
    /// includes interruption. `halt` carries the abort signal so the
    /// buffered tool-call events emit before the consumer drops the
    /// stream (native-runtime's forked tool dispatch then runs within
    /// cleanup's 250ms grace).
    pub async fn stream_with_halt(
        &self,
        request: &LlmRequest,
        halt: tokio_util::sync::CancellationToken,
    ) -> Result<futures::stream::BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError>
    where
        P: Send + Sync + 'static,
        P::State: Send + 'static,
    {
        use futures::StreamExt;

        let prepared = self.compile(request)?;
        let response = self.executor.execute(&prepared).await?;
        #[allow(clippy::result_large_err)]
        let bytes = response
            .into_inner()
            .bytes_stream()
            .map(|chunk| chunk.map_err(transport_error));
        let frames = self.handle.framing.clone().frame(bytes);

        let protocol = self.protocol.clone();
        let state = protocol.initial(request);
        let stream = futures::stream::unfold(
            StreamState {
                protocol,
                state: Some(state),
                frames,
                pending: Vec::new(),
                done: false,
                halt,
            },
            |mut s| async move {
                loop {
                    // Drain pending events first (in order).
                    if let Some(item) = s.pending.pop() {
                        return Some((Ok(item), s));
                    }
                    if s.done {
                        return None;
                    }
                    let next = tokio::select! {
                        biased;
                        _ = s.halt.cancelled() => {
                            // Interrupted (Effect stream halt): flush the
                            // buffered parser state, then end.
                            s.done = true;
                            if let Some(state) = s.state.take() {
                                s.pending = s.protocol.on_halt(state);
                            }
                            s.frames = futures::stream::empty().boxed();
                            continue;
                        }
                        item = s.frames.next() => item,
                    };
                    match next {
                        Some(Ok(frame)) => {
                            let event = match s.protocol.decode_frame(&frame) {
                                Ok(Some(event)) => event,
                                Ok(None) => continue,
                                Err(e) => return Some((Err(e), s)),
                            };
                            let is_terminal = s.protocol.terminal(&event);
                            let state = match s.state.take() {
                                Some(state) => state,
                                None => continue,
                            };
                            let (state, events) = match s.protocol.step(state, &event) {
                                Ok(result) => result,
                                Err(e) => return Some((Err(e), s)),
                            };
                            s.state = Some(state);
                            if is_terminal {
                                s.done = true;
                                let mut all = events;
                                if let Some(state) = s.state.take() {
                                    all.extend(s.protocol.on_halt(state));
                                }
                                if all.is_empty() {
                                    continue;
                                }
                                // pending pops from the end: push reversed.
                                for event in all.into_iter().rev() {
                                    s.pending.push(event);
                                }
                            } else if !events.is_empty() {
                                for event in events.into_iter().rev() {
                                    s.pending.push(event);
                                }
                            }
                        }
                        Some(Err(e)) => return Some((Err(e), s)),
                        None => {
                            s.done = true;
                            let all = match s.state.take() {
                                Some(state) => s.protocol.on_halt(state),
                                None => Vec::new(),
                            };
                            if all.is_empty() {
                                return None;
                            }
                            for event in all.into_iter().rev() {
                                s.pending.push(event);
                            }
                        }
                    }
                }
            },
        );
        Ok(stream.boxed())
    }

    /// Generate a response: fold the event stream via the `LlmResponse`
    /// reducer; error if the stream ends without a terminal finish event.
    pub async fn generate(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError>
    where
        P: Send + Sync + 'static,
        P::State: Send + 'static,
    {
        use futures::StreamExt;

        let mut stream = self.stream(request).await?;
        let mut state = crate::schema::events::LlmResponse::empty();
        while let Some(item) = stream.next().await {
            state = crate::schema::events::LlmResponse::reduce(state, item?);
        }
        LlmResponse::complete(&state).ok_or_else(|| {
            LlmError::invalid("Provider stream ended without a terminal finish event")
        })
    }
}

struct StreamState<P: Protocol> {
    protocol: Arc<P>,
    state: Option<P::State>,
    frames: futures::stream::BoxStream<'static, Result<serde_json::Value, LlmError>>,
    pending: Vec<LlmEvent>,
    done: bool,
    halt: tokio_util::sync::CancellationToken,
}

fn transport_error(e: reqwest::Error) -> LlmError {
    LlmError::invalid(format!("Route.stream: {e}"))
}

fn upsert_header(headers: &mut Headers, name: String, value: String) {
    headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(&name));
    headers.push((name, value));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoProtocol;

    impl Protocol for EchoProtocol {
        const ID: &'static str = "echo";

        type State = ();

        fn lower_body(&self, _request: &LlmRequest) -> Result<serde_json::Value, LlmError> {
            Ok(json!({"ok": true}))
        }

        fn decode_frame(
            &self,
            _frame: &serde_json::Value,
        ) -> Result<Option<serde_json::Value>, LlmError> {
            Ok(None)
        }

        fn initial(&self, _request: &LlmRequest) {}

        fn step(
            &self,
            state: (),
            _event: &serde_json::Value,
        ) -> Result<((), Vec<LlmEvent>), LlmError> {
            Ok((state, Vec::new()))
        }
    }

    #[test]
    fn compile_renders_endpoint_and_auth_headers() {
        let handle = RouteHandle {
            id: "echo-route".to_string(),
            protocol_id: "echo".to_string(),
            endpoint: {
                let mut e = Endpoint::path("/v1/chat");
                e.base_url = Some("https://api.example.com".to_string());
                e
            },
            auth: Auth::bearer(crate::route::auth::value("test-token")),
            framing: Framing::Sse,
            defaults: RouteDefaults::default(),
        };
        let route = Route::new(handle, EchoProtocol);
        let request = LlmRequest::new(crate::schema::messages::ModelRef::new(
            "echo-model",
            "echo",
            route.handle.clone(),
        ));
        let prepared = route.compile(&request).unwrap();
        assert_eq!(prepared.method, "POST");
        assert!(prepared.url.ends_with("/v1/chat"));
        assert!(prepared
            .headers
            .iter()
            .any(|(n, v)| n == "authorization" && v == "Bearer test-token"));
        assert!(prepared
            .headers
            .iter()
            .any(|(n, v)| n == "content-type" && v == "application/json"));
    }
}
