//! The binary frames that carry a heap snapshot.
//!
//! A frame is `u32 LE data_offset`, then UTF-8 JSON metadata padded with spaces up to the
//! offset (the VM always uses 512), then the payload. Raw payload bytes are never logged.

use serde_json::Value;

/// The size the VM reserves for a frame's metadata, offset field included.
pub const METADATA_RESERVATION: usize = 512;

/// Splits a frame into its JSON metadata and its payload. `None` unless the offset, a `u32` in
/// little-endian order, satisfies `4 <= offset <= len`.
pub fn split_frame(frame: &[u8]) -> Option<(&[u8], &[u8])> {
    let head: [u8; 4] = frame.get(..4)?.try_into().ok()?;
    let offset = usize::try_from(u32::from_le_bytes(head)).ok()?;
    if offset < 4 || offset > frame.len() {
        return None;
    }
    Some((&frame[4..offset], &frame[offset..]))
}

/// One chunk of a snapshot, in the order it arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeapChunk {
    pub isolate_id: String,
    pub timestamp_ms: i64,
    /// Set on the final chunk only.
    pub last: bool,
    pub payload: Vec<u8>,
}

/// Reads a frame as a snapshot chunk. `Some` only when the metadata is a `streamNotify` on the
/// `HeapSnapshot` stream; any other frame, including UTF-8 JSON text sent as binary, is `None`.
pub fn decode_chunk(frame: &[u8]) -> Option<HeapChunk> {
    let (json, payload) = split_frame(frame)?;
    let meta: Value = serde_json::from_slice(json).ok()?;
    if meta.get("method")?.as_str()? != "streamNotify" {
        return None;
    }
    let params = meta.get("params")?;
    if params.get("streamId")?.as_str()? != "HeapSnapshot" {
        return None;
    }
    let event = params.get("event")?;
    Some(HeapChunk {
        isolate_id: event
            .get("isolate")
            .and_then(|i| i.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        timestamp_ms: event.get("timestamp").and_then(Value::as_i64).unwrap_or(0),
        last: event.get("last").and_then(Value::as_bool).unwrap_or(false),
        payload: payload.to_vec(),
    })
}

/// Builds a frame as the VM does: metadata padded with spaces to [`METADATA_RESERVATION`].
#[cfg(any(test, feature = "testing"))]
pub fn encode_frame(isolate_id: &str, timestamp_ms: i64, last: bool, payload: &[u8]) -> Vec<u8> {
    let mut meta = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "streamNotify",
        "params": {"streamId": "HeapSnapshot", "event": {
            "type": "Event", "kind": "HeapSnapshot",
            "isolate": {"type": "@Isolate", "id": isolate_id},
            "timestamp": timestamp_ms, "last": last,
        }},
    })
    .to_string()
    .into_bytes();
    let data_offset = METADATA_RESERVATION.max(4 + meta.len());
    meta.resize(data_offset - 4, b' ');
    let mut frame = Vec::with_capacity(data_offset + payload.len());
    frame.extend_from_slice(&u32::try_from(data_offset).expect("small").to_le_bytes());
    frame.extend_from_slice(&meta);
    frame.extend_from_slice(payload);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_frames_decode() {
        let f = encode_frame("isolates/1", 42, true, &[0xff, 0, 1]);
        assert_eq!(f.len(), METADATA_RESERVATION + 3);
        let c = decode_chunk(&f).unwrap();
        assert_eq!(c.isolate_id, "isolates/1");
        assert_eq!(c.timestamp_ms, 42);
        assert!(c.last);
        assert_eq!(c.payload, [0xff, 0, 1]);
    }

    #[test]
    fn split_frame_rejects_bad_offsets() {
        let mut f = vec![0u8; 16];
        for bad in [0u32, 3, 17] {
            f[..4].copy_from_slice(&bad.to_le_bytes());
            assert!(split_frame(&f).is_none(), "offset {bad}");
        }
        f[..4].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(split_frame(&f).unwrap().0.len(), 0);
        f[..4].copy_from_slice(&16u32.to_le_bytes());
        assert_eq!(split_frame(&f).unwrap().1.len(), 0);
        for short in [&[][..], &[1], &[4, 0, 0]] {
            assert!(split_frame(short).is_none());
        }
    }

    #[test]
    fn other_frames_are_not_chunks() {
        assert!(decode_chunk(br#"{"jsonrpc":"2.0","method":"streamNotify"}"#).is_none());
        // A well-formed frame on another stream.
        let mut meta =
            br#"{"method":"streamNotify","params":{"streamId":"GC","event":{}}}"#.to_vec();
        meta.resize(METADATA_RESERVATION - 4, b' ');
        let mut f = (METADATA_RESERVATION as u32).to_le_bytes().to_vec();
        f.extend_from_slice(&meta);
        f.push(b'x');
        assert!(decode_chunk(&f).is_none());
    }
}
