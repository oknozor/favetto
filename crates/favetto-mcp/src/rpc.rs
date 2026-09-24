//! Thin helpers over the `favetto-core` wire [`Response`].
//!
//! The daemon returns a JSON-RPC-style [`Response`] whose `result`/`error` are
//! mutually exclusive. This mirrors the reference supervisor's private
//! `ok_result` so both external clients unwrap a reply the same way.

use anyhow::Context;
use favetto_core::rpc::Response;
use serde_json::Value;

/// Unwrap a successful RPC response, surfacing the wire error otherwise.
pub fn ok_result(resp: &Response, method: &str) -> anyhow::Result<Value> {
    if let Some(error) = &resp.error {
        anyhow::bail!("{method} failed ({}): {}", error.code, error.message);
    }
    resp.result
        .clone()
        .with_context(|| format!("{method} returned no result"))
}
