//! AWS event-stream framing for Bedrock Converse
//! (TS `protocols/bedrock-event-stream.ts`).
//!
//! Bedrock streams responses using the AWS event-stream binary protocol —
//! each frame is
//! `[total-length:4][headers-length:4][prelude-crc:4][headers][payload][message-crc:4]`,
//! all big-endian. The Rust port implements the codec in-crate (no smithy
//! dependency): typed header values 0–8 per the AWS event-stream spec, CRC32
//! validation, and the same `:message-type` filtering as TS.
//!
//! Only `:message-type == "event"` frames are kept; the JSON payload is
//! decoded, the AWS padding field `p` is deleted, and the payload is
//! rewrapped as `{ "<event-type>": payload }` (e.g.
//! `{ "messageStart": {…} }`) so the protocol's chunk schema can stay a plain
//! discriminated record.

#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;

use futures::Stream;
use futures::StreamExt;

use crate::protocols::shared::{self, event_error};
use crate::schema::errors::LlmError;

/// A decoded header value (AWS event-stream value types 0–8).
#[derive(Debug, Clone, PartialEq)]
pub enum HeaderValue {
    /// Type 0.
    True,
    /// Type 1.
    False,
    /// Type 2.
    Byte(i8),
    /// Type 3.
    Short(i16),
    /// Type 4.
    Int(i32),
    /// Type 5.
    Long(i64),
    /// Type 6.
    Bytes(Vec<u8>),
    /// Type 7.
    String(String),
    /// Type 8.
    Timestamp(i64),
}

/// One decoded AWS event-stream message.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedFrame {
    pub headers: BTreeMap<String, HeaderValue>,
    pub payload: Vec<u8>,
}

/// The minimal frame is the 12-byte prelude plus the 4-byte message CRC.
const MIN_MESSAGE_LENGTH: usize = 16;
const PRELUDE_LENGTH: usize = 8;
const PRELUDE_CRC_LENGTH: usize = 12;
const CRC_LENGTH: usize = 4;

/// CRC32 (IEEE 802.3, reflected, poly `0xEDB88320`) — the AWS event-stream
/// checksum. Hand-rolled to avoid a new dependency.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn read_u32(bytes: &[u8]) -> Result<u32, String> {
    if bytes.len() < 4 {
        return Err("unexpected end of frame".to_string());
    }
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn parse_headers(bytes: &[u8]) -> Result<BTreeMap<String, HeaderValue>, String> {
    let mut headers = BTreeMap::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let name_length = bytes[offset] as usize;
        offset += 1;
        if offset + name_length + 1 > bytes.len() {
            return Err("invalid header name length".to_string());
        }
        let name = String::from_utf8_lossy(&bytes[offset..offset + name_length]).into_owned();
        offset += name_length;
        let value_type = bytes[offset];
        offset += 1;
        let (value, next) = match value_type {
            0 => (HeaderValue::True, offset),
            1 => (HeaderValue::False, offset),
            2 => (
                HeaderValue::Byte(*bytes.get(offset).ok_or("unexpected end of header value")? as i8),
                offset + 1,
            ),
            3 => {
                let raw: [u8; 2] = bytes
                    .get(offset..offset + 2)
                    .ok_or("unexpected end of header value")?
                    .try_into()
                    .unwrap();
                (HeaderValue::Short(i16::from_be_bytes(raw)), offset + 2)
            }
            4 => {
                let raw: [u8; 4] = bytes
                    .get(offset..offset + 4)
                    .ok_or("unexpected end of header value")?
                    .try_into()
                    .unwrap();
                (HeaderValue::Int(i32::from_be_bytes(raw)), offset + 4)
            }
            5 => {
                let raw: [u8; 8] = bytes
                    .get(offset..offset + 8)
                    .ok_or("unexpected end of header value")?
                    .try_into()
                    .unwrap();
                (HeaderValue::Long(i64::from_be_bytes(raw)), offset + 8)
            }
            6 => {
                let raw: [u8; 2] = bytes
                    .get(offset..offset + 2)
                    .ok_or("unexpected end of header value")?
                    .try_into()
                    .unwrap();
                let length = u16::from_be_bytes(raw) as usize;
                offset += 2;
                let data = bytes
                    .get(offset..offset + length)
                    .ok_or("unexpected end of header value")?
                    .to_vec();
                (HeaderValue::Bytes(data), offset + length)
            }
            7 => {
                let raw: [u8; 2] = bytes
                    .get(offset..offset + 2)
                    .ok_or("unexpected end of header value")?
                    .try_into()
                    .unwrap();
                let length = u16::from_be_bytes(raw) as usize;
                offset += 2;
                let data = bytes
                    .get(offset..offset + length)
                    .ok_or("unexpected end of header value")?;
                (
                    HeaderValue::String(String::from_utf8_lossy(data).into_owned()),
                    offset + length,
                )
            }
            8 => {
                let raw: [u8; 8] = bytes
                    .get(offset..offset + 8)
                    .ok_or("unexpected end of header value")?
                    .try_into()
                    .unwrap();
                (HeaderValue::Timestamp(i64::from_be_bytes(raw)), offset + 8)
            }
            other => return Err(format!("invalid header value type {other}")),
        };
        offset = next;
        headers.insert(name, value);
    }
    Ok(headers)
}

/// Decode exactly one complete AWS event-stream message (the `total_length`
/// leading bytes). Both the prelude CRC and the message CRC are validated.
pub fn decode_frame(frame: &[u8]) -> Result<DecodedFrame, String> {
    if frame.len() < MIN_MESSAGE_LENGTH {
        return Err(format!("frame length {} is too short", frame.len()));
    }
    let total_length = read_u32(frame)? as usize;
    if total_length != frame.len() {
        return Err(format!(
            "frame length {} does not match declared length {}",
            frame.len(),
            total_length
        ));
    }
    let headers_length = read_u32(&frame[4..])? as usize;
    let prelude_crc = read_u32(&frame[8..])?;
    if prelude_crc != crc32(&frame[..PRELUDE_LENGTH]) {
        return Err("prelude CRC mismatch".to_string());
    }
    if PRELUDE_CRC_LENGTH + headers_length + CRC_LENGTH > total_length {
        return Err(format!(
            "header length {headers_length} exceeds message length"
        ));
    }
    let headers = parse_headers(&frame[PRELUDE_CRC_LENGTH..PRELUDE_CRC_LENGTH + headers_length])?;
    let message_crc = read_u32(&frame[total_length - CRC_LENGTH..])?;
    if message_crc != crc32(&frame[..total_length - CRC_LENGTH]) {
        return Err("message CRC mismatch".to_string());
    }
    Ok(DecodedFrame {
        headers,
        payload: frame[PRELUDE_CRC_LENGTH + headers_length..total_length - CRC_LENGTH].to_vec(),
    })
}

/// Encode one AWS event-stream message (the inverse of [`decode_frame`]).
/// Used by unit tests and the golden replay harness to synthesize
/// event-stream bytes.
pub fn encode_frame(headers: &[(&str, HeaderValue)], payload: &[u8]) -> Vec<u8> {
    let mut header_bytes = Vec::new();
    for (name, value) in headers {
        let name = name.as_bytes();
        header_bytes.push(name.len() as u8);
        header_bytes.extend_from_slice(name);
        match value {
            HeaderValue::True => header_bytes.push(0),
            HeaderValue::False => header_bytes.push(1),
            HeaderValue::Byte(raw) => {
                header_bytes.push(2);
                header_bytes.push(*raw as u8);
            }
            HeaderValue::Short(raw) => {
                header_bytes.push(3);
                header_bytes.extend_from_slice(&raw.to_be_bytes());
            }
            HeaderValue::Int(raw) => {
                header_bytes.push(4);
                header_bytes.extend_from_slice(&raw.to_be_bytes());
            }
            HeaderValue::Long(raw) => {
                header_bytes.push(5);
                header_bytes.extend_from_slice(&raw.to_be_bytes());
            }
            HeaderValue::Bytes(data) => {
                header_bytes.push(6);
                header_bytes.extend_from_slice(&(data.len() as u16).to_be_bytes());
                header_bytes.extend_from_slice(data);
            }
            HeaderValue::String(data) => {
                header_bytes.push(7);
                header_bytes.extend_from_slice(&(data.len() as u16).to_be_bytes());
                header_bytes.extend_from_slice(data.as_bytes());
            }
            HeaderValue::Timestamp(raw) => {
                header_bytes.push(8);
                header_bytes.extend_from_slice(&raw.to_be_bytes());
            }
        }
    }
    let total_length = PRELUDE_CRC_LENGTH + header_bytes.len() + payload.len() + CRC_LENGTH;
    let mut frame = Vec::with_capacity(total_length);
    frame.extend_from_slice(&(total_length as u32).to_be_bytes());
    frame.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32(&frame).to_be_bytes());
    frame.extend_from_slice(&header_bytes);
    frame.extend_from_slice(payload);
    let message_crc = crc32(&frame);
    frame.extend_from_slice(&message_crc.to_be_bytes());
    frame
}

/// Convert one decoded frame into the rewrapped JSON frame, or `None` for
/// frames the protocol drops (non-`event` messages, non-string or missing
/// event types, empty payloads).
fn rewrap(route: &str, frame: DecodedFrame) -> Result<Option<serde_json::Value>, LlmError> {
    if frame.headers.get(":message-type") != Some(&HeaderValue::String("event".to_string())) {
        return Ok(None);
    }
    let Some(HeaderValue::String(event_type)) = frame.headers.get(":event-type") else {
        return Ok(None);
    };
    let payload = String::from_utf8_lossy(&frame.payload);
    if payload.is_empty() {
        return Ok(None);
    }
    let parsed = shared::parse_json(
        route,
        &payload,
        "Failed to parse Bedrock Converse event-stream payload",
    )?;
    // The AWS event stream pads short payloads with a `p` field. Drop it
    // before handing the object to the chunk schema.
    let mut value = parsed;
    if let Some(record) = value.as_object_mut() {
        record.remove("p");
    }
    Ok(Some(serde_json::json!({ event_type: value })))
}

/// Consume one network chunk: append to the buffer, then decode every
/// complete frame. Mirrors TS `appendChunk` + `consumeFrames`. Once a frame
/// fails to decode or parse, the stream is dead — the buffer is dropped so
/// later chunks never emit garbage (TS fails the Effect stream).
fn consume_chunk(
    route: &str,
    buffer: &mut Vec<u8>,
    chunk: &[u8],
) -> Vec<Result<serde_json::Value, LlmError>> {
    buffer.extend_from_slice(chunk);
    let mut out = Vec::new();
    let mut offset = 0usize;
    while buffer.len() - offset >= 4 {
        let declared = read_u32(&buffer[offset..]).unwrap_or(0) as usize;
        if buffer.len() - offset < declared {
            break;
        }
        let failed = match decode_frame(&buffer[offset..offset + declared]) {
            Ok(frame) => match rewrap(route, frame) {
                Ok(Some(value)) => {
                    out.push(Ok(value));
                    None
                }
                Ok(None) => None,
                Err(error) => Some(error),
            },
            Err(error) => Some(event_error(
                route,
                format!("Failed to decode Bedrock Converse event-stream frame: {error}"),
                None,
            )),
        };
        if let Some(error) = failed {
            buffer.clear();
            out.push(Err(error));
            return out;
        }
        offset += declared;
    }
    buffer.drain(..offset);
    out
}

/// AWS event-stream framing for Bedrock Converse (TS
/// `BedrockEventStream.framing`): response bytes in, JSON frames wrapped
/// under their `:event-type` header out.
pub fn framing<'a, S, B>(
    route: &'a str,
    bytes: S,
) -> impl Stream<Item = Result<serde_json::Value, LlmError>> + 'a
where
    S: Stream<Item = Result<B, LlmError>> + 'a,
    B: AsRef<[u8]>,
{
    struct Accum {
        buffer: Vec<u8>,
        failed: bool,
    }
    bytes
        .scan(
            Accum {
                buffer: Vec::new(),
                failed: false,
            },
            |state, chunk| {
                let mut out = if state.failed {
                    Vec::new()
                } else {
                    match chunk {
                        Ok(chunk) => consume_chunk(route, &mut state.buffer, chunk.as_ref()),
                        Err(error) => vec![Err(error)],
                    }
                };
                // Once a frame failed to decode or parse the stream is dead
                // (TS fails the Effect stream); suppress everything after the
                // first failure.
                if state.failed {
                    out.clear();
                } else if out.iter().any(|frame| frame.is_err()) {
                    state.failed = true;
                }
                async move { Some(out) }
            },
        )
        .map(futures::stream::iter)
        .flatten()
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use serde_json::json;

    use super::*;
    use crate::schema::errors::LlmErrorReason;

    const ROUTE: &str = "bedrock-converse";

    fn event_frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
        encode_frame(
            &[
                (":message-type", HeaderValue::String("event".to_string())),
                (":event-type", HeaderValue::String(event_type.to_string())),
            ],
            payload,
        )
    }

    async fn collect(
        chunks: Vec<Result<Vec<u8>, LlmError>>,
    ) -> Vec<Result<serde_json::Value, LlmError>> {
        let stream = framing(ROUTE, futures::stream::iter(chunks));
        futures::pin_mut!(stream);
        let mut frames = Vec::new();
        while let Some(frame) = stream.next().await {
            frames.push(frame);
        }
        frames
    }

    #[test]
    fn crc32_matches_known_vectors() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"a"), 0xE8B7_BE43);
    }

    #[test]
    fn decode_frame_round_trips_encoded_frames() {
        let frame = event_frame("messageStart", br#"{"role":"assistant"}"#);
        let decoded = decode_frame(&frame).unwrap();
        assert_eq!(
            decoded.headers.get(":event-type"),
            Some(&HeaderValue::String("messageStart".to_string()))
        );
        assert_eq!(decoded.headers.get(":message-type-value"), None);
        assert_eq!(decoded.payload, br#"{"role":"assistant"}"#.to_vec());
    }

    #[test]
    fn decode_frame_rejects_crc_corruption() {
        let mut frame = event_frame("messageStart", b"{}");
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        assert!(decode_frame(&frame).is_err());

        let mut frame = event_frame("messageStart", b"{}");
        let payload_index = frame.len() - 2;
        frame[payload_index] ^= 0xFF;
        assert!(decode_frame(&frame).is_err());

        let mut frame = event_frame("messageStart", b"{}");
        frame[7] ^= 0xFF;
        assert!(decode_frame(&frame).is_err());
    }

    #[test]
    fn decode_frame_rejects_oversized_header_lengths() {
        let mut frame = event_frame("messageStart", b"{}");
        // headers-length > total-length - overhead.
        frame[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_frame(&frame).is_err());
    }

    #[tokio::test]
    async fn framing_rewraps_event_frames_and_drops_the_padding_field() {
        let first = event_frame("messageStart", br#"{"role":"assistant","p":"pad"}"#);
        let second = event_frame("metadata", br#"{"usage":{"inputTokens":12},"p":"x"}"#);
        let mut bytes = first;
        bytes.extend_from_slice(&second);
        let frames = collect(vec![Ok(bytes)]).await;
        assert_eq!(
            frames,
            vec![
                Ok(json!({"messageStart": {"role": "assistant"}})),
                Ok(json!({"metadata": {"usage": {"inputTokens": 12}}})),
            ],
        );
    }

    #[tokio::test]
    async fn framing_drops_non_event_messages_and_empty_payloads() {
        let exception = encode_frame(
            &[
                (
                    ":message-type",
                    HeaderValue::String("exception".to_string()),
                ),
                (":message-type-value", HeaderValue::Int(4)),
                (
                    ":content-type",
                    HeaderValue::String("application/json".to_string()),
                ),
            ],
            br#"{"message":"boom"}"#,
        );
        let empty = event_frame("contentBlockStop", b"");
        let mut bytes = exception;
        bytes.extend_from_slice(&empty);
        let frames = collect(vec![Ok(bytes)]).await;
        assert!(frames.is_empty());
    }

    #[tokio::test]
    async fn framing_buffers_across_chunk_boundaries() {
        let first = event_frame("messageStart", br#"{"role":"assistant"}"#);
        let second = event_frame("metadata", br#"{"usage":{"inputTokens":1}}"#);
        let mut bytes = first;
        bytes.extend_from_slice(&second);
        let mid = 5;
        let frames = collect(vec![Ok(bytes[..mid].to_vec()), Ok(bytes[mid..].to_vec())]).await;
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0].as_ref().unwrap(),
            &json!({"messageStart": {"role": "assistant"}})
        );
        assert_eq!(
            frames[1].as_ref().unwrap(),
            &json!({"metadata": {"usage": {"inputTokens": 1}}})
        );
    }

    #[tokio::test]
    async fn framing_reports_decode_errors_once() {
        // A complete-but-bogus frame declaring less than the minimum
        // message overhead.
        let bogus: [u8; 8] = [0, 0, 0, 8, 0xDE, 0xAD, 0xBE, 0xEF];
        let good = event_frame("metadata", br#"{}"#);
        let mut bytes = good.clone();
        bytes.extend_from_slice(&bogus);
        let frames = collect(vec![Ok(bytes), Ok(good)]).await;
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_ok());
        assert!(frames[1].is_err());
        match &frames[1].as_ref().unwrap_err().reason {
            LlmErrorReason::InvalidProviderOutput { message, route, .. } => {
                assert!(message.contains("Failed to decode Bedrock Converse event-stream frame"));
                assert!(message.contains("too short"));
                assert_eq!(route.as_deref(), Some(ROUTE));
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
    }

    #[tokio::test]
    async fn framing_propagates_transport_errors() {
        let frames = collect(vec![Err(LlmError::invalid("connection reset"))]).await;
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].as_ref().unwrap_err().reason,
            LlmErrorReason::InvalidRequest {
                message: "connection reset".to_string(),
                parameter: None,
                classification: None,
                provider_metadata: None,
                http: None,
            },
        );
    }
}
