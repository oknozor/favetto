//! Agent runtime: drives a tool-calling LLM loop through the ToolRegistry.
//!
//! The loop is model-agnostic behind the [`ModelBackend`] trait. M2 ships a
//! deterministic [`MockBackend`] (driven by a skill's scripted steps) so the whole
//! pipeline — skill → allowlist → registry → MCP server → result → next turn — can
//! be exercised end-to-end without an API key. A real OpenAI backend is
//! feature-gated behind `--features openai`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use favetto_core::model::{ChatMessage, MessageRole};

use crate::config::FavettoConfig;
use crate::skills::{MockStep, Skill};
use crate::tool_registry::ToolRegistry;

/// A model's response to the conversation: either a tool call or a final answer.
pub enum ModelResponse {
    ToolCall {
        server: String,
        tool: String,
        arguments: Value,
    },
    Final {
        text: String,
    },
}

/// Any model that can pick the next action from a conversation.
#[async_trait]
pub trait ModelBackend: Send + Sync {
    async fn next(&self, messages: &[ChatMessage]) -> anyhow::Result<ModelResponse>;
}

/// Deterministic backend driven by a skill's `[[agent.steps]]` list.
pub struct MockBackend {
    steps: Mutex<VecDeque<MockStep>>,
}

impl MockBackend {
    pub fn new(steps: Vec<MockStep>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
        }
    }
}

#[async_trait]
impl ModelBackend for MockBackend {
    async fn next(&self, _messages: &[ChatMessage]) -> anyhow::Result<ModelResponse> {
        let step = self.steps.lock().unwrap().pop_front();
        match step {
            Some(s) if s.kind == "tool" => Ok(ModelResponse::ToolCall {
                server: s.server.unwrap_or_default(),
                tool: s.tool.unwrap_or_default(),
                arguments: s.args.unwrap_or(Value::Null),
            }),
            Some(s) if s.kind == "final" => Ok(ModelResponse::Final {
                text: s.text.unwrap_or_default(),
            }),
            Some(other) => anyhow::bail!("unknown mock step kind: {:?}", other.kind),
            None => Ok(ModelResponse::Final {
                text: "(mock backend has no more steps)".to_string(),
            }),
        }
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
        })
    }
}

#[cfg(feature = "openai")]
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

    /// Run a skill to completion, returning its output JSON.
    pub async fn run(&self, skill: &Skill, input: Value) -> anyhow::Result<Value> {
        let tools = self.registry.allowlisted(&skill.config.tools.allow);
        let tool_descs = describe_tools(&tools);

        let mut messages = vec![
            ChatMessage::new(
                MessageRole::System,
                format!(
                    "{}\n\nYou may call these tools (JSON array of {{server, tool, description, input_schema}}):\n{}",
                    skill.prompt, tool_descs
                ),
            ),
            ChatMessage::new(MessageRole::User, input.to_string()),
        ];

        for _ in 0..skill.config.agent.max_iterations {
            match self.backend.next(&messages).await? {
                ModelResponse::Final { text } => {
                    return Ok(serde_json::json!({ "output": text }));
                }
                ModelResponse::ToolCall {
                    server,
                    tool,
                    arguments,
                } => {
                    let result = self.registry.call(&server, &tool, arguments.clone()).await?;
                    messages.push(ChatMessage::new(
                        MessageRole::Assistant,
                        format!("call {server}.{tool}({arguments})"),
                    ));
                    messages.push(ChatMessage::new(MessageRole::Tool, result));
                }
            }
        }

        anyhow::bail!(
            "skill {} exceeded max_iterations ({})",
            skill.name,
            skill.config.agent.max_iterations
        )
    }
}

/// Serialize the allowed tools into a compact JSON array for the model prompt.
fn describe_tools(tools: &[crate::tool_registry::ToolRef]) -> String {
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

/// Build a backend from a skill's `agent` config plus the global config's
/// providers. Returns an error for backends that aren't compiled in.
pub fn build_backend(skill: &Skill, config: &FavettoConfig) -> anyhow::Result<Arc<dyn ModelBackend>> {
    let model = skill.config.agent.model.as_str();

    if model == "mock" {
        return Ok(Arc::new(MockBackend::new(skill.config.agent.steps.clone())));
    }

    if let Some(backend) = openai_backend(model, config) {
        return Ok(backend);
    }

    anyhow::bail!(
        "unknown or unavailable model backend '{}' (use \"mock\", or enable the 'openai' feature)",
        model
    )
}

/// Build a backend for the interactive TUI chat.
///
/// Uses the skill's model, falling back to the global `[agent]` model, then to
/// [`EchoBackend`] so the chat always works offline.
pub fn build_chat_backend(skill: Option<&Skill>, config: &FavettoConfig) -> Arc<dyn ModelBackend> {
    let model = skill
        .filter(|s| s.config.agent.model.starts_with("openai"))
        .map(|s| s.config.agent.model.as_str())
        .or(config.agent.model.as_deref());

    if let Some(m) = model {
        if let Some(backend) = openai_backend(m, config) {
            return backend;
        }
    }

    Arc::new(EchoBackend)
}

/// Construct an OpenAI backend from a model string like `"openai"` or
/// `"openai:gpt-4o"`, resolving the API key/model from the global config
/// `[providers]` table (or the `OPENAI_API_KEY` env var as a last resort).
#[allow(unused_variables)] // `config` is only read by the feature-gated openai backend
fn openai_backend(model: &str, config: &FavettoConfig) -> Option<Arc<dyn ModelBackend>> {
    if !model.starts_with("openai") {
        return None;
    }

    #[cfg(feature = "openai")]
    {
        let provider = config
            .providers
            .get("openai")
            .or_else(|| config.provider("openai").map(|(_, p)| p));

        let api_key = provider
            .and_then(|p| p.api_key.clone())
            .or_else(|| {
                provider
                    .and_then(|p| p.api_key_env.as_deref())
                    .and_then(|name| std::env::var(name).ok())
            })
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())?;

        let model_name = model
            .strip_prefix("openai:")
            .map(str::to_string)
            .or_else(|| provider.and_then(|p| p.model.clone()))
            .unwrap_or_else(|| "gpt-4o".to_string());

        let base_url = provider.and_then(|p| p.base_url.clone());

        match OpenAiBackend::new(model_name, api_key, base_url) {
            Ok(backend) => return Some(Arc::new(backend)),
            Err(e) => tracing::warn!(error = %e, "openai backend unavailable; falling back to echo"),
        }
    }

    None
}

#[cfg(feature = "openai")]
mod openai {
    use super::*;

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
            let msgs: Vec<Value> = messages
                .iter()
                .map(|m| {
                    let role = match m.role {
                        MessageRole::System => "system",
                        MessageRole::User => "user",
                        MessageRole::Assistant => "assistant",
                        MessageRole::Tool => "tool",
                    };
                    serde_json::json!({ "role": role, "content": m.content })
                })
                .collect();

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

            if let Some(rest) = text.trim().strip_prefix("TOOL:") {
                let (call, args) = rest.split_once(' ').unwrap_or((rest, "{}"));
                let (server, tool) = call.split_once('.').unwrap_or(("", call));
                let arguments = serde_json::from_str::<Value>(args).unwrap_or(Value::Null);
                return Ok(ModelResponse::ToolCall {
                    server: server.to_string(),
                    tool: tool.to_string(),
                    arguments,
                });
            }

            Ok(ModelResponse::Final { text })
        }
    }
}
