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
        let bytes = encode(&item)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        dst.reserve(4 + bytes.len());
        dst.put_u32_le(bytes.len() as u32);
        dst.put_slice(&bytes);
        Ok(())
    }
}
