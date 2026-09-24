//! Shared MessagePack-frame ↔ WebSocket payload conversion.
//!
//! Both the client (`favetto-tui`'s `client::ws_connect`) and the daemon
//! (`transport::serve_socket`) speak the same wire format
//! over WebSocket: a [`Frame`] travels as a single binary (or text) message
//! carrying its msgpack encoding, and a Close message ends the stream.
//!
//! The two sides use different `Message` types (`tokio-tungstenite` vs `axum`),
//! so a fully generic adapter is not worth it. Instead this module owns the
//! payload-level conversion — encode/decode plus the "stream closed" error — and
//! each call site keeps only its own match over its own message type.

use crate::rpc::Frame;
use crate::wire::{decode, encode, WireError};

/// Decode a frame from a WebSocket binary payload.
pub fn inbound_binary(bytes: &[u8]) -> Result<Frame, WireError> {
    decode(bytes)
}

/// Decode a frame from a WebSocket text payload.
pub fn inbound_text(text: &str) -> Result<Frame, WireError> {
    decode(text.as_bytes())
}

/// Encode a frame into a WebSocket binary payload.
pub fn outbound(frame: &Frame) -> Result<Vec<u8>, WireError> {
    encode(frame)
}

/// The error that ends the inbound stream when a peer sends a Close message.
pub fn closed() -> WireError {
    WireError::Other("connection closed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::Request;

    fn request() -> Frame {
        Frame::Request(Request {
            id: 7,
            method: "system.ping".to_string(),
            params: serde_json::json!({ "nested": [1, 2, 3] }),
        })
    }

    /// A binary frame must survive an encode/decode round trip unchanged.
    #[test]
    fn binary_round_trip_preserves_the_frame() {
        let frame = request();
        let bytes = outbound(&frame).unwrap();
        let decoded = inbound_binary(&bytes).unwrap();
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::to_value(&frame).unwrap()
        );
    }

    /// Text messages carry the same msgpack payload as binary ones, so a
    /// non-msgpack text payload is reported as a decode error rather than
    /// silently ignored.
    #[test]
    fn text_payload_is_decoded_as_msgpack() {
        let err = inbound_text("not msgpack").unwrap_err();
        assert!(matches!(err, WireError::Decode(_)), "got {err}");
    }

    /// Malformed payloads surface the framing error instead of panicking.
    #[test]
    fn invalid_binary_payload_is_an_error() {
        let err = inbound_binary(b"not msgpack").unwrap_err();
        assert!(matches!(err, WireError::Decode(_)), "got {err}");
    }

    /// Close always maps to a terminal error carrying a stable message.
    #[test]
    fn closed_is_a_terminal_error() {
        let err = closed();
        assert_eq!(err.to_string(), "connection closed");
    }
}
