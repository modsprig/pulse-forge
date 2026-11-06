//! IPC wire protocol (roadmap PF-405): versioned, length-delimited JSON
//! frames over a byte-stream transport.
//!
//! Frame layout: `u32 LE length | utf-8 JSON document`, hard-capped at
//! [`MAX_FRAME_BYTES`] (4 MiB per the architecture contract). The first
//! frame in each direction is a handshake carrying protocol version and
//! capabilities; a version mismatch is a typed error, not a guess. Every
//! request carries a client-chosen `id` echoed in the response, enabling
//! pipelining and reconnect correlation.
//!
//! Deviation from the brief (recorded in ADR-013): JSON frames instead of
//! protobuf — consistent with the zero-dependency baseline (ADR-011) while
//! preserving the contract's framing, versioning and bound requirements.

use std::io::{Read, Write};

use pf_types::json::{self, JsonValue};
use pf_types::{ErrorCode, PfError};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Write one frame.
pub fn write_frame(w: &mut dyn Write, value: &JsonValue) -> Result<(), PfError> {
    let body = value.to_json_string();
    let bytes = body.as_bytes();
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(PfError::new(
            ErrorCode::ResourceExhausted,
            format!("frame of {} bytes exceeds {MAX_FRAME_BYTES}", bytes.len()),
        ));
    }
    let len = u32::try_from(bytes.len())
        .map_err(|_| PfError::new(ErrorCode::ResourceExhausted, "frame too large"))?;
    w.write_all(&len.to_le_bytes()).map_err(PfError::from)?;
    w.write_all(bytes).map_err(PfError::from)?;
    w.flush().map_err(PfError::from)
}

/// Read one frame with the global size cap. `read` is transport-specific.
pub fn read_frame(r: &mut dyn Read) -> Result<JsonValue, PfError> {
    let mut len_buf = [0u8; 4];
    read_exact_or_eof(r, &mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 {
        return Err(PfError::new(
            ErrorCode::InvalidArgument,
            "zero-length frame",
        ));
    }
    if len > MAX_FRAME_BYTES {
        return Err(PfError::new(
            ErrorCode::ResourceExhausted,
            format!("peer frame of {len} bytes exceeds {MAX_FRAME_BYTES}"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .map_err(|e| PfError::new(ErrorCode::BackendFailure, format!("truncated frame: {e}")))?;
    let text = String::from_utf8(body)
        .map_err(|_| PfError::new(ErrorCode::InvalidArgument, "frame is not valid UTF-8"))?;
    json::parse(&text).map_err(PfError::from)
}

/// Read that treats clean EOF as `Ok(())` (client disconnect is normal).
// `filled` is bounded by buf.len().
#[allow(clippy::arithmetic_side_effects)]
fn read_exact_or_eof(r: &mut dyn Read, buf: &mut [u8]) -> Result<(), PfError> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => {
                return Err(PfError::new(ErrorCode::Timeout, "client disconnected"));
            }
            Ok(0) => {
                return Err(PfError::new(
                    ErrorCode::BackendFailure,
                    "connection closed mid-frame",
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(PfError::from(e)),
        }
    }
    Ok(())
}

/// Build a client handshake frame.
pub fn client_handshake(client_name: &str) -> JsonValue {
    use pf_types::json::JsonValue as J;
    J::Object(vec![
        ("type".into(), J::Str("handshake".into())),
        (
            "protocol_version".into(),
            J::UInt(u64::from(PROTOCOL_VERSION)),
        ),
        ("client".into(), J::Str(client_name.into())),
    ])
}

/// Validate a received handshake; returns the server handshake to send.
pub fn accept_handshake(received: &JsonValue) -> Result<JsonValue, PfError> {
    use pf_types::json::JsonValue as J;
    let is_handshake = received.get("type").and_then(|v| v.as_str()) == Some("handshake");
    if !is_handshake {
        return Err(PfError::new(
            ErrorCode::InvalidArgument,
            "expected handshake as the first frame",
        ));
    }
    let version = received
        .get("protocol_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if version != u64::from(PROTOCOL_VERSION) {
        return Err(PfError::new(
            ErrorCode::InvalidArgument,
            format!("protocol version {version} not supported (server speaks {PROTOCOL_VERSION})"),
        )
        .with_remediation("update the client and daemon to matching versions"));
    }
    Ok(J::Object(vec![
        ("type".into(), J::Str("handshake_ok".into())),
        (
            "protocol_version".into(),
            J::UInt(u64::from(PROTOCOL_VERSION)),
        ),
        ("server".into(), J::Str("pulseforge-daemon".into())),
    ]))
}

/// Build a request frame.
pub fn request(id: u64, op: &str, args: JsonValue) -> JsonValue {
    use pf_types::json::JsonValue as J;
    J::Object(vec![
        ("type".into(), J::Str("request".into())),
        ("id".into(), J::UInt(id)),
        ("op".into(), J::Str(op.into())),
        ("args".into(), args),
    ])
}

/// Build a success response.
pub fn response_ok(id: u64, result: JsonValue) -> JsonValue {
    use pf_types::json::JsonValue as J;
    J::Object(vec![
        ("type".into(), J::Str("response".into())),
        ("id".into(), J::UInt(id)),
        ("ok".into(), J::Bool(true)),
        ("result".into(), result),
    ])
}

/// Build a typed error response (error never kills the connection).
pub fn response_err(id: u64, e: &PfError) -> JsonValue {
    use pf_types::json::JsonValue as J;
    let mut pairs = vec![
        ("type".into(), J::Str("response".into())),
        ("id".into(), J::UInt(id)),
        ("ok".into(), J::Bool(false)),
        (
            "error".into(),
            J::Object(vec![
                ("code".into(), J::Str(e.code.as_str().into())),
                ("message".into(), J::Str(e.message.clone())),
            ]),
        ),
    ];
    if let Some(r) = &e.remediation {
        pairs.push(("remediation".into(), J::Str(r.clone())));
    }
    J::Object(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip_through_a_buffer() {
        let msg = request(7, "list_sessions", JsonValue::Object(vec![]));
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        assert_eq!(buf.len(), 4 + msg.to_json_string().len());
        let mut cursor = std::io::Cursor::new(buf);
        let back = read_frame(&mut cursor).unwrap();
        assert_eq!(back.get("id").and_then(|v| v.as_u64()), Some(7));
        assert_eq!(
            back.get("op").and_then(|v| v.as_str()),
            Some("list_sessions")
        );
    }

    #[test]
    fn oversized_frames_are_rejected_in_both_directions() {
        let huge = JsonValue::Str("x".repeat(MAX_FRAME_BYTES));
        let mut buf = Vec::new();
        assert!(write_frame(&mut buf, &huge).is_err());

        // Peer-sent length header over the cap: rejected before allocation.
        let mut evil = Vec::new();
        evil.extend_from_slice(&u32::try_from(MAX_FRAME_BYTES + 1).unwrap().to_le_bytes());
        let mut cursor = std::io::Cursor::new(evil);
        let err = read_frame(&mut cursor).unwrap_err();
        assert_eq!(err.code, ErrorCode::ResourceExhausted);
    }

    #[test]
    fn zero_length_frame_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame(&mut cursor).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn truncated_frame_is_typed_error() {
        let msg = request(1, "x", JsonValue::Object(vec![]));
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        buf.truncate(buf.len() - 5);
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame(&mut cursor).unwrap_err();
        assert_eq!(err.code, ErrorCode::BackendFailure);
    }

    #[test]
    fn clean_disconnect_surfaces_as_timeout_code() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        let err = read_frame(&mut cursor).unwrap_err();
        assert_eq!(err.code, ErrorCode::Timeout);
    }

    #[test]
    fn handshake_version_mismatch_is_rejected() {
        let wrong =
            json::parse(r#"{"type":"handshake","protocol_version":99,"client":"x"}"#).unwrap();
        let err = accept_handshake(&wrong).unwrap_err();
        assert!(err.message.contains("not supported"));
        let right = client_handshake("t");
        assert!(accept_handshake(&right).is_ok());
    }

    #[test]
    fn non_handshake_first_frame_rejected() {
        let not = request(1, "op", JsonValue::Object(vec![]));
        assert!(accept_handshake(&not).is_err());
    }

    #[test]
    fn error_responses_carry_code_and_remediation() {
        let e = PfError::new(ErrorCode::PermissionDenied, "nope")
            .with_remediation("try a different pid");
        let r = response_err(3, &e);
        assert_eq!(r.get("id").and_then(|v| v.as_u64()), Some(3));
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            r.get("error")
                .and_then(|x| x.get("code"))
                .and_then(|v| v.as_str()),
            Some("PERMISSION_DENIED")
        );
        assert!(r.get("remediation").is_some());
    }
}
