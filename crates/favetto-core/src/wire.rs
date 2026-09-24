//! Length-prefixed MessagePack framing.
//!
//! Over a raw byte stream (Unix socket / plain TCP) each frame is:
//!
//! ```text
//! [u32 LE length][msgpack-encoded Frame]
//! ```
//!
//! Over WebSocket each frame is a single binary (or text) WS message containing the
//! msgpack-encoded [`Frame`] — WS already provides message boundaries, so no extra
//! length prefix is used there. [`FrameCodec`] implements the raw-stream form and is
//! reused by the daemon, the TUI client, and future transports.

use bytes::{BufMut, BytesMut};
use thiserror::Error;
use tokio_util::codec::{Decoder, Encoder};

use crate::rpc::Frame;

/// Upper bound on a single frame, to avoid unbounded allocation from a bad peer.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

/// Errors produced while framing/unframing. This is also the unified error type
/// carried by the transport streams in the daemon and TUI client.
#[derive(Debug, Error)]
pub enum WireError {
    #[error("frame too large: {0} bytes (max {MAX_FRAME_LEN})")]
    FrameTooLarge(usize),
    #[error("msgpack encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("msgpack decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Encode a frame to msgpack bytes (no length prefix — see [`FrameCodec`]).
pub fn encode(frame: &Frame) -> Result<Vec<u8>, WireError> {
    let bytes = rmp_serde::to_vec_named(frame)?;
    if bytes.len() > MAX_FRAME_LEN {
        return Err(WireError::FrameTooLarge(bytes.len()));
    }
    Ok(bytes)
}

/// Decode a frame from msgpack bytes.
pub fn decode(buf: &[u8]) -> Result<Frame, WireError> {
    Ok(rmp_serde::from_slice(buf)?)
}

/// `tokio-util` codec that adds/removes the u32 LE length prefix for raw byte streams.
pub struct FrameCodec;

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([src[0], src[1], src[2], src[3]]) as usize;
        if len > MAX_FRAME_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                WireError::FrameTooLarge(len),
            ));
        }
        if src.len() < 4 + len {
            src.reserve(4 + len - src.len());
            return Ok(None);
        }
        let _ = src.split_to(4);
        let payload = src.split_to(len);
        let frame = rmp_serde::from_slice(&payload)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Some(frame))
    }
}

impl Encoder<Frame> for FrameCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: Frame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let bytes =
            encode(&item).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        dst.reserve(4 + bytes.len());
        dst.put_u32_le(bytes.len() as u32);
        dst.put_slice(&bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{Notification, Request, Response, RpcErrorObject};
    use bytes::BytesMut;
    use proptest::prelude::*;
    use tokio_util::codec::{Decoder, Encoder};

    type Json = serde_json::Value;

    fn arb_json(depth: u32) -> BoxedStrategy<Json> {
        if depth == 0 {
            prop_oneof![
                Just(Json::Null),
                any::<bool>().prop_map(Json::Bool),
                any::<i64>().prop_map(|n| Json::Number(n.into())),
                "[ -~]{0,24}".prop_map(Json::String),
            ]
            .boxed()
        } else {
            prop_oneof![
                arb_json(0),
                // `serde_json::Value::Object` maps deterministically (its `Map`
                // is ordered without `preserve_order`), so byte equality stays
                // stable across encode/decode.
                prop::collection::btree_map("[a-z]{1,8}", arb_json(depth - 1), 0..4)
                    .prop_map(|m| Json::Object(m.into_iter().collect())),
                prop::collection::vec(arb_json(depth - 1), 0..4).prop_map(Json::Array),
            ]
            .boxed()
        }
    }

    // NOTE: `Some(Json::Null)` collapses to `None` through serde's `Option`
    // handling; exclude it so the round-trip property is meaningful.
    fn arb_opt_json(depth: u32) -> BoxedStrategy<Option<Json>> {
        prop_oneof![
            Just(None),
            arb_json(depth)
                .prop_filter("non-null", |v| !v.is_null())
                .prop_map(Some),
        ]
        .boxed()
    }

    fn arb_frame() -> impl Strategy<Value = Frame> {
        let req = (any::<u64>(), "[a-z.]{0,16}", arb_json(3))
            .prop_map(|(id, method, params)| Frame::Request(Request { id, method, params }));
        let resp = (
            any::<u64>(),
            arb_opt_json(3),
            proptest::option::of((any::<i32>(), "[ -~]{0,32}", arb_opt_json(2))),
        )
            .prop_map(|(id, result, error)| {
                Frame::Response(Response {
                    id,
                    result,
                    error: error.map(|(code, message, data)| RpcErrorObject {
                        code,
                        message,
                        data,
                    }),
                })
            });
        let note = ("[a-z.]{0,16}", arb_json(3))
            .prop_map(|(method, params)| Frame::Notification(Notification { method, params }));
        prop_oneof![req, resp, note]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn frame_round_trips_via_msgpack(frame in arb_frame()) {
            let bytes = encode(&frame).expect("encode");
            let decoded = decode(&bytes).expect("decode");
            prop_assert_eq!(
                serde_json::to_value(&frame).unwrap(),
                serde_json::to_value(&decoded).unwrap()
            );
            prop_assert_eq!(encode(&decoded).expect("re-encode"), bytes);
        }

        #[test]
        fn frame_codec_round_trips_through_framing(frame in arb_frame()) {
            let mut codec = FrameCodec;
            let mut buf = BytesMut::new();
            codec.encode(frame, &mut buf).expect("encode");
            let mut decoded = Vec::new();
            while let Some(item) = codec.decode(&mut buf).expect("decode") {
                decoded.push(item);
            }
            prop_assert_eq!(decoded.len(), 1);
        }

        #[test]
        fn frame_codec_never_panics_on_arbitrary_bytes(
            bytes in prop::collection::vec(any::<u8>(), 0..512)
        ) {
            let mut codec = FrameCodec;
            let mut buf = BytesMut::from(&bytes[..]);
            if let Ok(Some(frame)) = codec.decode(&mut buf) {
                prop_assert!(encode(&frame).is_ok());
            }
        }
    }

    #[test]
    fn frame_codec_rejects_length_above_max() {
        for len in [MAX_FRAME_LEN as u32 + 1, u32::MAX] {
            let mut buf = BytesMut::new();
            // Only the length prefix: no payload, so a buggy decoder would try
            // to reserve `len` bytes and return `Ok(None)`.
            buf.extend_from_slice(&len.to_le_bytes());
            let mut codec = FrameCodec;
            let err = codec
                .decode(&mut buf)
                .expect_err("length above MAX_FRAME_LEN must be rejected");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
            // The rejected header must not have over-allocated.
            assert!(buf.capacity() < 4096, "capacity grew to {}", buf.capacity());
        }
    }

    #[test]
    fn frame_codec_waits_for_a_full_header() {
        let mut codec = FrameCodec;
        for n in 0..4 {
            let mut buf = BytesMut::from(&vec![0u8; n][..]);
            assert!(codec.decode(&mut buf).unwrap().is_none());
        }
    }
}
