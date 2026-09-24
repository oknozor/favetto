//! Minimal MCP / JSON-RPC 2.0 envelope types.
//!
//! The MCP stdio transport frames each message as a single line of JSON, so the
//! server only needs a handful of envelope shapes: a request, a response, and an
//! error object. Notifications (requests without an `id`) never get a response.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol revision this server implements (the `initialize` lifecycle).
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Revisions this server can echo back when a client requests them. Anything
/// else falls back to [`PROTOCOL_VERSION`].
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-03-26", "2025-06-18", "2025-11-25"];

/// Standard JSON-RPC error codes, mirroring `favetto_core::rpc::error_code`.
pub mod error_code {
    pub const PARSE: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL: i32 = -32603;
}

/// A JSON-RPC request as sent by an MCP client.
///
/// `id` is absent for notifications. `params` defaults to JSON `null` so a
/// parameterless call (e.g. `ping`) decodes without a `params` field.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(default)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// A JSON-RPC response. Exactly one of `result`/`error` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A successful response, echoing the request `id` unchanged.
pub fn response_ok(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: Some(result),
        error: None,
    }
}

/// An error response, echoing the request `id` unchanged.
pub fn response_err(id: Value, code: i32, message: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.into(),
            data: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_decodes_with_and_without_params() {
        let with: JsonRpcRequest =
            serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"ping","params":{}}))
                .unwrap();
        assert_eq!(with.method, "ping");
        assert_eq!(with.id, Some(json!(1)));
        assert_eq!(with.params, json!({}));

        let without: JsonRpcRequest =
            serde_json::from_value(json!({"jsonrpc":"2.0","id":"a","method":"ping"})).unwrap();
        assert_eq!(without.id, Some(json!("a")));
        assert_eq!(without.params, Value::Null);
    }

    #[test]
    fn request_id_is_optional_for_notifications() {
        let n: JsonRpcRequest =
            serde_json::from_value(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .unwrap();
        assert!(n.id.is_none());
    }

    #[test]
    fn responses_echo_string_and_numeric_ids() {
        let numeric = response_ok(json!(7), json!({"ok": true}));
        let text = serde_json::to_string(&numeric).unwrap();
        assert!(text.contains("\"id\":7"), "{text}");
        assert!(text.contains("\"result\""), "{text}");
        assert!(!text.contains("\"error\""), "{text}");

        let string = response_ok(json!("abc"), json!(null));
        let text = serde_json::to_string(&string).unwrap();
        assert!(text.contains("\"id\":\"abc\""), "{text}");
    }

    #[test]
    fn error_response_carries_the_code_and_message() {
        let response = response_err(json!(1), error_code::METHOD_NOT_FOUND, "no such method");
        assert!(response.result.is_none());
        let error = response.error.expect("error object");
        assert_eq!(error.code, error_code::METHOD_NOT_FOUND);
        assert_eq!(error.message, "no such method");
        assert!(error.data.is_none());
    }
}
