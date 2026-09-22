//! Decode a streaming HTTP response body into provider-protocol frames.
//!
//! Port of `route/framing.ts` (M2.6). [`Framing`] is the byte-stream-shaped
//! seam between transport and protocol: [`Framing::Sse`] runs the SSE channel
//! decoder and JSON-decodes each `data:` payload into a `serde_json::Value`;
//! [`Framing::AwsEventStream`] decodes length-prefixed binary event records
//! (implemented in M2.11). The frame type is opaque to this layer; the
//! protocol's `decode_frame` step turns a frame into a typed chunk.

#![allow(clippy::result_large_err)]

use futures::stream::BoxStream;
use futures::StreamExt;

use crate::protocols::bedrock_converse;
use crate::protocols::bedrock_event_stream;
use crate::protocols::shared::sse_framing;
use crate::schema::errors::{LlmError, LlmErrorReason};

/// Byte-stream → frame decoding strategy for one route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Server-Sent Events — used by every JSON-streaming HTTP provider.
    Sse,
    /// AWS event stream — length-prefixed binary frames with CRC checksums.
    AwsEventStream,
}

impl Framing {
    /// Stable id for the framing implementation (TS `Framing.id`).
    pub fn id(&self) -> &'static str {
        match self {
            Framing::Sse => "sse",
            Framing::AwsEventStream => "aws-event-stream",
        }
    }

    /// Decode a response byte stream into JSON frames.
    pub fn frame<S, B>(&self, bytes: S) -> BoxStream<'static, Result<serde_json::Value, LlmError>>
    where
        S: futures::Stream<Item = Result<B, LlmError>> + Send + 'static,
        B: AsRef<[u8]> + 'static,
    {
        match self {
            Framing::Sse => sse_framing(bytes)
                .map(|frame| frame.and_then(|data| parse_sse_frame(&data)))
                .boxed(),
            // AWS event-stream binary frame decoder
            // (bedrock-event-stream.ts).
            Framing::AwsEventStream => {
                bedrock_event_stream::framing(bedrock_converse::ADAPTER, bytes).boxed()
            }
        }
    }
}

/// Parse one SSE `data:` payload into a JSON frame. The TS reference parses
/// the payload in the protocol's event decode step, reporting an
/// `InvalidProviderOutput` with the raw frame; this layer reports the same
/// reason (without route context, which only the pipeline knows).
fn parse_sse_frame(data: &str) -> Result<serde_json::Value, LlmError> {
    serde_json::from_str(data).map_err(|_| LlmError {
        module: "Framing".to_string(),
        method: "frame".to_string(),
        reason: LlmErrorReason::InvalidProviderOutput {
            message: "Invalid stream event".to_string(),
            route: None,
            raw: Some(data.to_string()),
            provider_metadata: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_bytes(
        chunks: Vec<Result<Vec<u8>, LlmError>>,
    ) -> BoxStream<'static, Result<serde_json::Value, LlmError>> {
        Framing::Sse.frame(futures::stream::iter(chunks))
    }

    #[tokio::test]
    async fn sse_framing_yields_json_frames() {
        let stream = frame_bytes(vec![Ok(b"data: {\"type\": \"ping\"}\n\n".to_vec())]);
        let frames: Vec<_> = stream.collect().await;
        assert_eq!(frames, vec![Ok(serde_json::json!({"type": "ping"}))]);
    }

    #[tokio::test]
    async fn sse_framing_drops_done_and_empty_frames() {
        let stream = frame_bytes(vec![Ok(
            b"data: [DONE]\n\ndata: \n\ndata: {\"ok\": true}\n\n".to_vec(),
        )]);
        let frames: Vec<_> = stream.collect().await;
        assert_eq!(frames, vec![Ok(serde_json::json!({"ok": true}))]);
    }

    #[tokio::test]
    async fn sse_framing_errors_on_non_json_payload() {
        let stream = frame_bytes(vec![Ok(b"data: not json\n\n".to_vec())]);
        let frames: Vec<_> = stream.collect().await;
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_err());
        let error = frames[0].as_ref().unwrap_err();
        assert_eq!(error.module, "Framing");
        assert!(matches!(
            error.reason,
            LlmErrorReason::InvalidProviderOutput { .. }
        ));
    }

    #[tokio::test]
    async fn aws_event_stream_frames_decode_to_json() {
        use crate::protocols::bedrock_event_stream::{encode_frame, HeaderValue};

        let first = encode_frame(
            &[
                (":message-type", HeaderValue::String("event".to_string())),
                (
                    ":event-type",
                    HeaderValue::String("messageStart".to_string()),
                ),
            ],
            br#"{"role":"assistant","p":"pad"}"#,
        );
        let second = encode_frame(
            &[
                (":message-type", HeaderValue::String("event".to_string())),
                (":event-type", HeaderValue::String("metadata".to_string())),
            ],
            br#"{"usage":{"inputTokens":12},"p":"x"}"#,
        );
        let mut bytes = first;
        bytes.extend_from_slice(&second);
        let frames: Vec<_> = Framing::AwsEventStream
            .frame(futures::stream::iter(vec![Ok(bytes)]))
            .collect()
            .await;
        assert_eq!(
            frames,
            vec![
                Ok(serde_json::json!({"messageStart": {"role": "assistant"}})),
                Ok(serde_json::json!({"metadata": {"usage": {"inputTokens": 12}}})),
            ],
        );
    }

    #[test]
    fn ids_match_ts() {
        assert_eq!(Framing::Sse.id(), "sse");
        assert_eq!(Framing::AwsEventStream.id(), "aws-event-stream");
    }
}
