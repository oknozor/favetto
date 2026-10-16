//! The built-in `http.request` action: perform one HTTP request and capture its
//! status, headers, and (bounded) body.
//!
//! Parameters (`[action.params]`):
//!
//! | Key | Type | Description |
//! |-----|------|-------------|
//! | `method` | string | HTTP method, default `GET`. |
//! | `url` | string | Absolute request URL (required). |
//! | `query` | table | Query parameters appended to the URL. |
//! | `headers` | table | Request headers; values may interpolate `$VAR` / `${VAR}` from the environment. |
//! | `body` | string or value | Request body; strings are sent verbatim, other values as JSON. |
//! | `timeout_ms` | integer | Whole-request timeout, default 30 000 ms. |
//! | `fail_on_error` | bool | Fail the action on a non-2xx response, default `true`. Set `false` to keep the status in `outputs` and branch on it with `[action.when]`. |
//! | `user_agent` | string | `User-Agent` header, default `favetto/<version>`. |
//!
//! By default a non-2xx response fails the action with
//! [`FailureKind::Infrastructure`](favetto_core::model::FailureKind::Infrastructure):
//! the message carries the status and a bounded body excerpt, and the task
//! writes no child manifest. Set `fail_on_error = false` to instead record
//! `status` and let an `[action.when]` predicate decide. Transport faults (DNS,
//! connect, timeout, unreadable body) and non-2xx responses both fail. Every
//! request sends a `User-Agent`, which GitHub and other UA-strict APIs require.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::{Method, Response};
use serde_json::{json, Map, Value};

use super::{Action, ActionContext, ActionError, ActionOutcome};

/// Default whole-request timeout when `timeout_ms` is absent.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on the response-body excerpt embedded in a non-2xx failure message, so a
/// large error page cannot bloat the persisted `error`/`failure.message`.
const MAX_ERROR_BODY_CHARS: usize = 512;

/// The built-in action that performs an HTTP request.
pub struct HttpRequestAction;

impl Action for HttpRequestAction {
    fn id(&self) -> &str {
        "http.request"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a ActionContext,
    ) -> BoxFuture<'a, Result<ActionOutcome, ActionError>> {
        Box::pin(async move { run(ctx).await })
    }
}

async fn run(ctx: &ActionContext) -> Result<ActionOutcome, ActionError> {
    let params = ctx
        .params
        .as_object()
        .ok_or_else(|| ActionError::invalid("http.request params must be a table"))?;

    tracing::debug!(
        task_id = %ctx.task_id,
        cwd = %ctx.cwd.display(),
        "executing http.request action"
    );

    let url = string_param(params, "url")
        .ok_or_else(|| ActionError::invalid("http.request requires a `url` param"))?;
    let method = string_param(params, "method").unwrap_or_else(|| "GET".to_string());
    let method = Method::from_bytes(method.to_ascii_uppercase().as_bytes())
        .map_err(|_| ActionError::invalid(format!("invalid HTTP method '{method}'")))?;
    let timeout = params
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TIMEOUT);
    let fail_on_error = match params.get("fail_on_error") {
        None => true,
        Some(Value::Bool(value)) => *value,
        Some(_) => {
            return Err(ActionError::invalid(
                "http.request `fail_on_error` must be a boolean",
            ))
        }
    };
    let user_agent = string_param(params, "user_agent")
        .unwrap_or_else(|| format!("favetto/{}", env!("CARGO_PKG_VERSION")));

    let client = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(user_agent)
        .build()
        .map_err(|e| ActionError::infrastructure(format!("failed to build HTTP client: {e}")))?;

    let mut request = client.request(method.clone(), &url);

    if let Some(query) = params.get("query").and_then(Value::as_object) {
        let pairs: Vec<(String, String)> = query
            .iter()
            .map(|(key, value)| (key.clone(), scalar_to_string(value)))
            .collect();
        request = request.query(&pairs);
    }
    if let Some(headers) = params.get("headers").and_then(Value::as_object) {
        for (name, raw) in headers {
            let Some(rendered) = raw.as_str() else {
                return Err(ActionError::invalid(format!(
                    "header '{name}' value must be a string"
                )));
            };
            let value = interpolate_env(rendered, &ctx.env).map_err(ActionError::invalid)?;
            request = request.header(name, value);
        }
    }
    if let Some(body) = params.get("body") {
        request = match body {
            Value::String(text) => request.body(text.clone()),
            other => request.json(other),
        };
    }

    let response = request
        .send()
        .await
        .map_err(|e| ActionError::infrastructure(format!("{method} {url} failed: {e}")))?;

    let status = response.status();
    let headers: Map<String, Value> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                Value::String(value.to_str().unwrap_or_default().to_string()),
            )
        })
        .collect();
    let (body_text, truncated) = read_bounded(response, ctx.max_output_bytes).await?;

    if fail_on_error && !status.is_success() {
        let snippet: String = body_text.chars().take(MAX_ERROR_BODY_CHARS).collect();
        let snippet = snippet.trim();
        let detail = if snippet.is_empty() {
            String::new()
        } else {
            format!(": {snippet}")
        };
        return Err(ActionError::infrastructure(format!(
            "{method} {url} -> {status}{detail}"
        )));
    }

    let body = serde_json::from_str::<Value>(&body_text).unwrap_or(Value::String(body_text));
    let items = match &body {
        Value::Array(_) => body.clone(),
        Value::Object(map) => map.get("items").cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    };

    let mut summary = format!("{method} {url} -> {}", status.as_u16());
    if let Some(items) = items.as_array() {
        summary.push_str(&format!(", {} item(s)", items.len()));
    }

    let mut outputs = Map::new();
    outputs.insert("status".to_string(), json!(status.as_u16()));
    outputs.insert("headers".to_string(), Value::Object(headers));
    outputs.insert("body".to_string(), body);
    outputs.insert("items".to_string(), items);
    outputs.insert("truncated".to_string(), Value::Bool(truncated));

    Ok(ActionOutcome {
        summary,
        outputs: Value::Object(outputs),
    })
}

/// Read a response body, capping it at `max` bytes. Returns the decoded text and
/// whether the cap truncated it.
async fn read_bounded(mut response: Response, max: usize) -> Result<(String, bool), ActionError> {
    let mut body: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        let chunk = response.chunk().await.map_err(|e| {
            ActionError::infrastructure(format!("failed to read response body: {e}"))
        })?;
        let Some(chunk) = chunk else {
            break;
        };
        let remaining = max.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok((String::from_utf8_lossy(&body).into_owned(), truncated))
}

/// A required/optional string parameter.
fn string_param(params: &Map<String, Value>, name: &str) -> Option<String> {
    params.get(name).and_then(Value::as_str).map(str::to_string)
}

/// Render a query value as a string. Strings are taken verbatim; scalars use
/// their JSON spelling; `null` becomes empty.
fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Expand `$NAME` and `${NAME}` references from `env`. A referenced variable
/// that is not set is an error, so a missing secret never sends a truncated
/// header silently. The expanded value is only ever placed on the outbound
/// request, never in the action's output.
fn interpolate_env(input: &str, env: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('{') => {
                chars.next();
                let mut name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }
                if !closed {
                    return Err(format!("unterminated ${{...}} in '{input}'"));
                }
                out.push_str(env.get(&name).ok_or_else(|| {
                    format!("environment variable '{name}' referenced by a header is not set")
                })?);
            }
            Some(next) if next.is_ascii_alphanumeric() || *next == '_' => {
                let mut name = String::new();
                while let Some(next) = chars.peek() {
                    if next.is_ascii_alphanumeric() || *next == '_' {
                        name.push(*next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(env.get(&name).ok_or_else(|| {
                    format!("environment variable '{name}' referenced by a header is not set")
                })?);
            }
            _ => out.push('$'),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use uuid::Uuid;

    /// A one-shot local HTTP server. Returns its base URL and a handle yielding
    /// the raw request bytes it received.
    async fn serve_status_once(
        status: u16,
        body: &'static str,
        content_type: &'static str,
    ) -> (String, Handle) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let reason = http::StatusCode::from_u16(status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("");
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        });
        (format!("http://{addr}"), handle)
    }

    async fn serve_once(body: &'static str, content_type: &'static str) -> (String, Handle) {
        serve_status_once(200, body, content_type).await
    }

    type Handle = tokio::task::JoinHandle<String>;

    fn context(_url: &str, params: Value) -> ActionContext {
        ActionContext {
            params,
            cwd: std::env::temp_dir(),
            task_id: Uuid::new_v4(),
            max_output_bytes: 1024 * 1024,
            env: BTreeMap::new(),
        }
    }

    async fn execute(ctx: &ActionContext) -> Result<ActionOutcome, ActionError> {
        HttpRequestAction.execute(ctx).await
    }

    #[tokio::test]
    async fn performs_a_get_and_captures_status_and_items() {
        let (url, handle) = serve_once("[{\"number\":1},{\"number\":2}]", "application/json").await;
        let ctx = context(
            &url,
            json!({
                "method": "get",
                "url": url,
                "query": { "state": "open" },
                "headers": { "Accept": "application/vnd.github+json" },
            }),
        );
        let outcome = execute(&ctx).await.unwrap();
        let request = handle.await.unwrap();

        assert!(request.starts_with("GET /?state=open "), "{request}");
        assert!(
            request.contains("accept: application/vnd.github+json")
                || request.contains("Accept: application/vnd.github+json"),
            "{request}"
        );
        assert_eq!(outcome.outputs["status"], json!(200));
        assert_eq!(outcome.outputs["items"].as_array().unwrap().len(), 2);
        assert!(outcome.summary.contains("-> 200"), "{}", outcome.summary);
        assert!(outcome.summary.contains("2 item"), "{}", outcome.summary);
    }

    #[tokio::test]
    async fn interpolates_env_vars_into_headers_only() {
        let (url, handle) = serve_once("{}", "application/json").await;
        let mut ctx = context(
            &url,
            json!({
                "url": url,
                "headers": { "Authorization": "Bearer ${TOKEN}" },
            }),
        );
        ctx.env
            .insert("TOKEN".to_string(), "s3cr3t-value".to_string());
        let outcome = execute(&ctx).await.unwrap();
        let request = handle.await.unwrap();

        assert!(request.contains("Bearer s3cr3t-value"), "{request}");
        // The secret is never persisted into the result.
        assert!(!outcome.outputs.to_string().contains("s3cr3t-value"));
        assert!(!outcome.summary.contains("s3cr3t-value"));
    }

    #[tokio::test]
    async fn missing_env_var_is_an_invalid_input_error() {
        let (url, _handle) = serve_once("{}", "application/json").await;
        let ctx = context(
            &url,
            json!({ "url": url, "headers": { "Authorization": "Bearer $TOKEN" } }),
        );
        let err = execute(&ctx).await.unwrap_err();
        assert_eq!(err.kind, favetto_core::model::FailureKind::InvalidInput);
        assert!(err.message.contains("TOKEN"), "{}", err.message);
    }

    #[tokio::test]
    async fn non_json_body_is_kept_as_text() {
        let (url, _handle) = serve_once("pong", "text/plain").await;
        let ctx = context(&url, json!({ "url": url }));
        let outcome = execute(&ctx).await.unwrap();
        assert_eq!(outcome.outputs["body"], json!("pong"));
        assert_eq!(outcome.outputs["items"], json!(null));
    }

    #[tokio::test]
    async fn body_is_capped_at_max_output_bytes() {
        let (url, _handle) = serve_once("0123456789", "text/plain").await;
        let mut ctx = context(&url, json!({ "url": url }));
        ctx.max_output_bytes = 4;
        let outcome = execute(&ctx).await.unwrap();
        assert_eq!(outcome.outputs["body"], json!("0123"));
        assert_eq!(outcome.outputs["truncated"], json!(true));
    }

    #[tokio::test]
    async fn non_2xx_is_an_infrastructure_failure_by_default() {
        let (url, _handle) =
            serve_status_once(403, r#"{"message":"Forbidden"}"#, "application/json").await;
        let ctx = context(&url, json!({ "url": url }));
        let err = execute(&ctx).await.unwrap_err();
        assert_eq!(err.kind, favetto_core::model::FailureKind::Infrastructure);
        assert!(err.message.contains("403"), "{}", err.message);
        assert!(err.message.contains("Forbidden"), "{}", err.message);
    }

    #[tokio::test]
    async fn fail_on_error_false_keeps_a_non_2xx_response_as_success() {
        let (url, _handle) =
            serve_status_once(403, r#"{"message":"Forbidden"}"#, "application/json").await;
        let ctx = context(&url, json!({ "url": url, "fail_on_error": false }));
        let outcome = execute(&ctx).await.unwrap();
        assert_eq!(outcome.outputs["status"], json!(403));
        assert_eq!(outcome.outputs["items"], json!(null));
    }

    #[tokio::test]
    async fn request_carries_a_default_user_agent() {
        let (url, handle) = serve_once("{}", "application/json").await;
        let ctx = context(&url, json!({ "url": url }));
        let _ = execute(&ctx).await.unwrap();
        let request = handle.await.unwrap().to_lowercase();
        assert!(
            request.contains(&format!(
                "user-agent: favetto/{}",
                env!("CARGO_PKG_VERSION")
            )),
            "{request}"
        );
    }

    #[tokio::test]
    async fn user_agent_param_overrides_the_default() {
        let (url, handle) = serve_once("{}", "application/json").await;
        let ctx = context(&url, json!({ "url": url, "user_agent": "custom-agent/9" }));
        let _ = execute(&ctx).await.unwrap();
        let request = handle.await.unwrap().to_lowercase();
        assert!(request.contains("user-agent: custom-agent/9"), "{request}");
    }

    #[tokio::test]
    async fn invalid_fail_on_error_is_invalid() {
        let (url, _handle) = serve_once("{}", "application/json").await;
        let ctx = context(&url, json!({ "url": url, "fail_on_error": "yes" }));
        let err = execute(&ctx).await.unwrap_err();
        assert_eq!(err.kind, favetto_core::model::FailureKind::InvalidInput);
    }

    #[tokio::test]
    async fn missing_url_is_invalid() {
        let ctx = ActionContext {
            params: json!({ "method": "GET" }),
            cwd: std::env::temp_dir(),
            task_id: Uuid::new_v4(),
            max_output_bytes: 1024,
            env: BTreeMap::new(),
        };
        let err = execute(&ctx).await.unwrap_err();
        assert_eq!(err.kind, favetto_core::model::FailureKind::InvalidInput);
    }

    /// Keep the `Arc` import exercised like a real registry would.
    #[test]
    fn action_is_object_safe() {
        let action: Arc<dyn Action> = Arc::new(HttpRequestAction);
        assert_eq!(action.id(), "http.request");
    }
}
