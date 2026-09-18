//! Agent runtime: drives a tool-calling LLM loop through the ToolRegistry.
//!
//! The loop is model-agnostic behind the [`ModelBackend`] trait. A real
//! OpenAI-compatible backend (OpenAI, DeepSeek, …) is always available via
//! [`OpenAiBackend`]; [`EchoBackend`] is a keyless fallback for offline development.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use favetto_core::model::{ChatMessage, MessageRole};

use crate::config::FavettoConfig;
use crate::tool_registry::ToolRegistry;

/// A model's response to the conversation: either a tool call or a final answer.
pub enum ModelResponse {
    ToolCall {
        server: String,
        tool: String,
        arguments: Value,
        reasoning: Option<String>,
    },
    Final {
        text: String,
        reasoning: Option<String>,
    },
}

/// Any model that can pick the next action from a conversation.
#[async_trait]
pub trait ModelBackend: Send + Sync {
    async fn next(&self, messages: &[ChatMessage]) -> anyhow::Result<ModelResponse>;

    /// Like [`next`](Self::next), but streams answer tokens via `on_delta` and
    /// reasoning tokens via `on_reasoning` as they arrive. The default falls back to
    /// a single non-streamed [`next`](Self::next) call.
    async fn next_streaming(
        &self,
        messages: &[ChatMessage],
        on_delta: &(dyn for<'a> Fn(&'a str) + Send + Sync),
        on_reasoning: &(dyn for<'a> Fn(&'a str) + Send + Sync),
    ) -> anyhow::Result<ModelResponse> {
        let _ = (on_delta, on_reasoning);
        self.next(messages).await
    }
}

/// Interactive backend that echoes the last user message. Used for the TUI chat when
/// no real model is configured — lets the full send/receive path run without a key.
pub struct EchoBackend;

#[async_trait]
impl ModelBackend for EchoBackend {
    async fn next(&self, messages: &[ChatMessage]) -> anyhow::Result<ModelResponse> {
        let last = messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .map(|m| m.content.as_str())
            .unwrap_or("");
        Ok(ModelResponse::Final {
            text: format!("(echo) {last}"),
            reasoning: None,
        })
    }
}

pub use openai::OpenAiBackend;

/// The tool-calling loop itself.
pub struct Runtime {
    registry: ToolRegistry,
    backend: Arc<dyn ModelBackend>,
}

impl Runtime {
    pub fn new(registry: ToolRegistry, backend: Arc<dyn ModelBackend>) -> Self {
        Self { registry, backend }
    }

    /// Run the tool-calling loop with an explicit system prompt and tool set.
    pub async fn run_with_prompt(
        &self,
        prompt: &str,
        tools: &[crate::tool_registry::ToolRef],
        input: Value,
        max_iterations: usize,
    ) -> anyhow::Result<Value> {
        let mut messages = vec![
            ChatMessage::new(MessageRole::System, system_prompt(prompt, tools)),
            ChatMessage::new(MessageRole::User, input.to_string()),
        ];
        self.run_turn(&mut messages, max_iterations).await?;
        let text = messages.last().map(|m| m.content.clone()).unwrap_or_default();
        Ok(serde_json::json!({ "output": text }))
    }

    /// Drive one conversational turn: call the model repeatedly until it produces a
    /// final answer, dispatching any tool calls through the registry and feeding the
    /// results back. Appends to `messages`.
    pub async fn run_turn(
        &self,
        messages: &mut Vec<ChatMessage>,
        max_iterations: usize,
    ) -> anyhow::Result<()> {
        self.run_turn_streaming(messages, max_iterations, &|_| {}, &|_| {})
            .await
    }

    /// [`run_turn`](Self::run_turn) with streaming: answer tokens are emitted via
    /// `on_delta` and reasoning tokens via `on_reasoning` as the model produces them.
    pub async fn run_turn_streaming(
        &self,
        messages: &mut Vec<ChatMessage>,
        max_iterations: usize,
        on_delta: &(dyn for<'a> Fn(&'a str) + Send + Sync),
        on_reasoning: &(dyn for<'a> Fn(&'a str) + Send + Sync),
    ) -> anyhow::Result<()> {
        for _ in 0..max_iterations {
            match self
                .backend
                .next_streaming(messages, on_delta, on_reasoning)
                .await?
            {
                ModelResponse::Final { text, reasoning } => {
                    messages.push(
                        ChatMessage::new(MessageRole::Assistant, text).with_reasoning(reasoning),
                    );
                    return Ok(());
                }
                ModelResponse::ToolCall {
                    server,
                    tool,
                    arguments,
                    reasoning,
                } => {
                    let result = self.registry.call(&server, &tool, arguments.clone()).await?;
                    messages.push(
                        ChatMessage::new(
                            MessageRole::Assistant,
                            format!("call {server}.{tool}({arguments})"),
                        )
                        .with_reasoning(reasoning),
                    );
                    // Feed the result back as a user message rather than the reserved
                    // `tool` role — the OpenAI-compatible API rejects bare `tool`
                    // messages without the accompanying tool_call protocol.
                    messages.push(ChatMessage::new(
                        MessageRole::User,
                        format!("Tool result for {server}.{tool}: {result}"),
                    ));
                }
            }
        }

        anyhow::bail!("agent exceeded max_iterations ({max_iterations})")
    }
}

/// Build the system prompt for a model, including the tool catalogue and the
/// plain-text protocol for invoking a tool.
pub fn system_prompt(prompt: &str, tools: &[crate::tool_registry::ToolRef]) -> String {
    let tool_descs = describe_tools(tools);
    format!(
        "{prompt}\n\nYou have access to these tools (JSON array of {{server, tool, description, input_schema}}):\n{tool_descs}\n\n\
         To call a tool, reply with exactly one line of the form:\n\
         TOOL: <server>.<tool> <arguments-json>\n\
         for example: TOOL: filesystem.list_dir {{\"path\": \".\"}}\n\
         You will then receive the tool result and may continue. Otherwise reply normally.\n\n\
         Always respond with well-structured Markdown: use headings (##, ###) to organize sections, \
         bullet or numbered lists for enumerations, fenced code blocks (```) for code/commands, and \
         bold/italic for emphasis where it aids clarity. Every user-facing reply must be valid Markdown."
    )
}

/// Serialize the allowed tools into a compact JSON array for the model prompt.
pub fn describe_tools(tools: &[crate::tool_registry::ToolRef]) -> String {
    let arr: Vec<Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "server": t.server_id,
                "tool": t.name(),
                "description": t.tool.description.as_deref().unwrap_or(""),
                "input_schema": t.tool.input_schema,
            })
        })
        .collect();
    serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_string())
}

/// Default agent loop length for a task/chat turn.
pub const DEFAULT_MAX_ITERATIONS: usize = 40;

/// Connect every configured MCP server (non-fatally) and return a registry.
pub async fn connect_registry(config: &FavettoConfig) -> ToolRegistry {
    let servers = config.mcp_servers().unwrap_or_default();
    let mut sessions = Vec::new();
    for cfg in &servers {
        // A missing/broken MCP server must not fail an unrelated task: skip it and
        // let the agent use the tools that *are* available.
        match crate::mcp_client::McpSession::connect(cfg).await {
            Ok(session) => sessions.push(session),
            Err(e) => tracing::warn!(
                server = %cfg.name,
                error = %e,
                "skipping unavailable MCP server"
            ),
        }
    }
    ToolRegistry::new(sessions)
}

/// Run a catalog task: build the backend from the task's `model` and use the
/// task's Markdown prompt, exposing every globally-configured MCP server.
pub async fn run_task(
    def: &crate::tasks::TaskDef,
    config: &FavettoConfig,
    input: Value,
) -> anyhow::Result<Value> {
    let registry = connect_registry(config).await;
    let tools = registry.all_tools();

    let backend = match openai_backend(&def.model, config) {
        Ok(Some(backend)) => backend,
        Ok(None) => anyhow::bail!(
            "no provider configured for model '{}' — add a [providers.<name>] entry with kind = \"openai\"",
            def.model
        ),
        Err(e) => anyhow::bail!("cannot use model '{}': {e}", def.model),
    };

    let runtime = Runtime::new(registry, backend);
    runtime
        .run_with_prompt(&def.prompt, &tools, input, DEFAULT_MAX_ITERATIONS)
        .await
}

/// Build a backend for an explicit model string (e.g. a catalog task's `model`),
/// falling back to [`EchoBackend`] when it doesn't resolve.
pub fn backend_for_model(model: Option<&str>, config: &FavettoConfig) -> Arc<dyn ModelBackend> {
    if let Some(m) = model {
        match openai_backend(m, config) {
            Ok(Some(backend)) => return backend,
            Ok(None) => {} // not an OpenAI-compatible provider; fall through to echo
            Err(e) => tracing::warn!(model = %m, error = %e, "model backend unavailable; falling back to echo"),
        }
    }
    Arc::new(EchoBackend)
}

/// Construct an OpenAI-compatible backend from a model string like `"openai"`,
/// `"openai:gpt-4o"`, or `"deepseek:deepseek-v4-flash"`, resolving the provider's
/// API key, model, and base URL from the global config `[providers]` table (with
/// `OPENAI_API_KEY` as a last-resort key).
///
/// Returns `Ok(None)` when `model` doesn't name an OpenAI-compatible provider, and
/// `Err` when it does but the backend couldn't be constructed (e.g. no API key).
fn openai_backend(model: &str, config: &FavettoConfig) -> anyhow::Result<Option<Arc<dyn ModelBackend>>> {
    let (provider_name, model_part) = match model.split_once(':') {
        Some((name, m)) => (name, Some(m)),
        None => (model, None),
    };

    let Some(provider) = config.providers.get(provider_name) else {
        return Ok(None);
    };
    if provider.kind != "openai" {
        return Ok(None);
    }

    let api_key = provider
        .api_key
        .clone()
        .or_else(|| {
            provider
                .api_key_env
                .as_deref()
                .and_then(|name| std::env::var(name).ok())
        })
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "provider '{provider_name}' has no API key — set `api_key` to the key, or \
                 `api_key_env` to the name of an environment variable that holds it"
            )
        })?;

    let model_name = model_part
        .map(str::to_string)
        .or_else(|| provider.model.clone())
        .unwrap_or_else(|| "gpt-4o".to_string());

    let base_url = provider.base_url.clone();

    Ok(Some(Arc::new(OpenAiBackend::new(
        model_name,
        api_key,
        base_url,
    )?)))
}

mod openai {
    use super::*;
    use futures_util::StreamExt;

    /// Thin OpenAI chat-completions backend (one `reqwest` POST per turn).
    ///
    /// The model replies in plain text: either a final answer, or a
    /// `TOOL: <server>.<tool> {args}` directive that the loop dispatches. Native
    /// function-calling is a follow-up; this keeps the backend dependency-light.
    pub struct OpenAiBackend {
        model: String,
        client: reqwest::Client,
        api_key: String,
        base_url: String,
    }

    impl OpenAiBackend {
        pub fn new(model: String, api_key: String, base_url: Option<String>) -> anyhow::Result<Self> {
            Ok(Self {
                model,
                client: reqwest::Client::new(),
                api_key,
                base_url: base_url
                    .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            })
        }
    }

    #[async_trait]
    impl ModelBackend for OpenAiBackend {
        async fn next(&self, messages: &[ChatMessage]) -> anyhow::Result<ModelResponse> {
            let msgs = self.messages(messages);
            let body = serde_json::json!({ "model": self.model, "messages": msgs });
            let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

            let resp: Value = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            let text = resp["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let reasoning = resp["choices"][0]["message"]["reasoning_content"]
                .as_str()
                .map(str::to_string);

            Ok(parse_model_output(&text, reasoning))
        }

        async fn next_streaming(
            &self,
            messages: &[ChatMessage],
            on_delta: &(dyn for<'a> Fn(&'a str) + Send + Sync),
            on_reasoning: &(dyn for<'a> Fn(&'a str) + Send + Sync),
        ) -> anyhow::Result<ModelResponse> {
            let msgs = self.messages(messages);
            let body =
                serde_json::json!({ "model": self.model, "messages": msgs, "stream": true });
            let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await?
                .error_for_status()?;

            // Parse the SSE stream, emitting tokens as they arrive.
            let mut full = String::new();
            let mut reasoning = String::new();
            let mut buf = String::new();
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(pos) = buf.find('\n') {
                    let line: String = buf.drain(..=pos).collect();
                    let line = line.trim();
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(data) else {
                        continue;
                    };
                    let delta = &v["choices"][0]["delta"];
                    if let Some(c) = delta["content"].as_str() {
                        if !c.is_empty() {
                            full.push_str(c);
                            on_delta(c);
                        }
                    }
                    if let Some(r) = delta["reasoning_content"].as_str() {
                        if !r.is_empty() {
                            reasoning.push_str(r);
                            on_reasoning(r);
                        }
                    }
                }
            }

            let reasoning = (!reasoning.is_empty()).then_some(reasoning);
            Ok(parse_model_output(&full, reasoning))
        }
    }

    impl OpenAiBackend {
        fn messages(&self, messages: &[ChatMessage]) -> Vec<Value> {
            messages
                .iter()
                .map(|m| {
                    let role = match m.role {
                        MessageRole::System => "system",
                        MessageRole::User => "user",
                        MessageRole::Assistant => "assistant",
                        // The plain-text tool protocol feeds results back as `user`
                        // messages; map any `tool`-role message defensively to `user`.
                        MessageRole::Tool => "user",
                    };
                    serde_json::json!({ "role": role, "content": m.content })
                })
                .collect()
        }
    }
}

/// Parse the model's plain-text output into a final answer or a tool call.
fn parse_model_output(text: &str, reasoning: Option<String>) -> ModelResponse {
    if let Some(rest) = text.trim().strip_prefix("TOOL:") {
        let rest = rest.trim();
        let (call, args) = rest.split_once(' ').unwrap_or((rest, "{}"));
        let (server, tool) = call.split_once('.').unwrap_or(("", call));
        let arguments = serde_json::from_str::<Value>(args).unwrap_or(Value::Null);
        return ModelResponse::ToolCall {
            server: server.to_string(),
            tool: tool.to_string(),
            arguments,
            reasoning,
        };
    }
    ModelResponse::Final {
        text: text.to_string(),
        reasoning,
    }
}
