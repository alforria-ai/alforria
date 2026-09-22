//! PTY websocket wire protocol — port of `packages/core/src/pty/protocol.ts`.
//!
//! Outbound frames are raw UTF-8 terminal chunks. One control frame — a 0x00
//! byte followed by UTF-8 JSON — carries the absolute output cursor after
//! replay so clients can resume later.

/// `REPLAY_CHUNK` (`protocol.ts:13`).
pub const REPLAY_CHUNK: usize = 64 * 1024;

/// `metaFrame` (`protocol.ts:15-21`): `0x00` + `{"cursor":N}`.
pub fn meta_frame(cursor: u64) -> Vec<u8> {
    let mut out = vec![0u8];
    out.extend_from_slice(format!("{{\"cursor\":{cursor}}}").as_bytes());
    out
}

/// `chunks` (`protocol.ts:23-27`) — replay sliced into bounded frames.
/// TS slices JS strings, i.e. UTF-16 code units.
pub fn chunks(data: &[u16]) -> Vec<String> {
    data.chunks(REPLAY_CHUNK)
        .map(String::from_utf16_lossy)
        .collect()
}

/// `decodeInput` (`protocol.ts:30-36`): binary frames must be valid UTF-8,
/// invalid input is dropped.
pub fn decode_input(message: &[u8]) -> Option<String> {
    String::from_utf8(message.to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_frame_is_a_null_byte_followed_by_json() {
        assert_eq!(meta_frame(42), b"\x00{\"cursor\":42}");
        assert_eq!(meta_frame(0), b"\x00{\"cursor\":0}");
    }

    #[test]
    fn chunks_slice_by_code_unit() {
        let text = "héllo".encode_utf16().collect::<Vec<u16>>();
        assert_eq!(chunks(&text), vec!["héllo".to_string()]);
        assert_eq!(chunks(&text).len(), 1, "short buffers stay in one frame");
        let long = vec![0x41u16; super::REPLAY_CHUNK + 1];
        let out = chunks(&long);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), super::REPLAY_CHUNK);
        assert_eq!(out[1].len(), 1);
    }

    #[test]
    fn decode_input_drops_invalid_utf8() {
        assert_eq!(decode_input(b"ok").as_deref(), Some("ok"));
        assert_eq!(decode_input(&[0xff, 0xfe]), None);
        // Invalid UTF-8 never decodes, even with a valid prefix.
        assert_eq!(decode_input(&[b'a', 0xff]), None);
    }
}
