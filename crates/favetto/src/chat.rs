//! Interactive LLM chat sessions, opened per-task from the TUI.
//!
//! A session holds a conversation plus a [`Runtime`] (registry + model backend), so
//! the chat is fully tool-enabled: the model is given the tool catalogue and its
//! tool calls are dispatched through the registry like a task run. Conversations are
//! persisted in SQLite (keyed by task id) so they survive daemon restarts.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use sqlx::SqlitePool;
use tokio::sync::Mutex;
use uuid::Uuid;

use favetto_core::model::{ChatMessage, ChatSession, MessageRole};

use crate::config::FavettoConfig;
use crate::db;
use crate::runtime::{self, Runtime};
use crate::tasks::TaskDef;

/// Conversation key for chats not attached to a task.
const GENERAL_CONVERSATION: &str = "general";

/// Owns all live chat sessions, keyed by session id.
pub struct ChatManager {
    db: SqlitePool,
    config: Arc<RwLock<FavettoConfig>>,
    catalog: Arc<RwLock<Vec<TaskDef>>>,
    sessions: Mutex<HashMap<String, ChatSessionState>>,
}

struct ChatSessionState {
    id: String,
    task_id: Option<String>,
    conversation_id: String,
    messages: Vec<ChatMessage>,
    runtime: Arc<Runtime>,
}

impl ChatManager {
    pub fn new(
        db: SqlitePool,
        config: Arc<RwLock<FavettoConfig>>,
        catalog: Arc<RwLock<Vec<TaskDef>>>,
    ) -> Self {
        Self {
            db,
            config,
            catalog,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Open a session for a task: load the task and its definition, build a
    /// tool-enabled runtime from the definition's model, and load (or seed) the
    /// conversation.
    pub async fn open(
        &self,
        pool: &SqlitePool,
        task_id: Option<String>,
    ) -> anyhow::Result<ChatSession> {
        let id = Uuid::new_v4().to_string();
        let conversation_id = task_id.clone().unwrap_or_else(|| GENERAL_CONVERSATION.to_string());

        const DEFAULT_PROMPT: &str =
            "You are the favetto's agent. Help the user work on their task.\n";
        let mut user_prompt = "Hello!".to_string();
        let mut def_name: Option<String> = None;
        let mut output: Option<String> = None;

        if let Some(tid) = &task_id {
            if let Ok(u) = Uuid::parse_str(tid) {
                if let Ok(Some(task)) = db::get_task(pool, u).await {
                    user_prompt = format!("Task: {} ({})", task.name, task.id);
                    output = task.output.and_then(|v| extract_output(&v));
                    def_name = Some(task.name);
                }
            }
        }

        let catalog = self.catalog.read().unwrap().clone();
        let def = def_name
            .as_deref()
            .and_then(|name| catalog.iter().find(|d| d.name == name).cloned());

        // Clone the config so we don't hold the read guard across `.await`s.
        let config = self.config.read().unwrap().clone();
        let model = def
            .as_ref()
            .map(|d| d.model.as_str())
            .or(config.agent.model.as_deref());
        let prompt = def
            .as_ref()
            .map(|d| d.prompt.clone())
            .unwrap_or_else(|| DEFAULT_PROMPT.to_string());

        let backend = runtime::backend_for_model(model, &config);
        let registry = runtime::connect_registry(&config).await;
        let tools = registry.all_tools();
        let system = runtime::system_prompt(&prompt, &tools);
        let runtime = Arc::new(Runtime::new(registry, backend));

        // Load any persisted conversation, otherwise seed a fresh one.
        let persisted = db::list_chat_messages(pool, &conversation_id).await?;
        let messages = if persisted.is_empty() {
            let mut messages = vec![
                ChatMessage::new(MessageRole::System, system),
                ChatMessage::new(MessageRole::User, user_prompt),
            ];
            // Surface the task's previous output (if any) so the conversation starts
            // with the result the agent already produced.
            if let Some(output) = output {
                messages.push(ChatMessage::new(MessageRole::Assistant, output));
            }
            db::save_chat_messages(pool, &conversation_id, &messages).await?;
            messages
        } else {
            persisted
        };

        self.sessions.lock().await.insert(
            id.clone(),
            ChatSessionState {
                id: id.clone(),
                task_id,
                conversation_id,
                messages,
                runtime,
            },
        );

        Ok(self.session_view(&id).await)
    }

    /// Send a user message and run one tool-calling turn with streaming: answer
    /// tokens are emitted via `on_delta` and reasoning tokens via `on_reasoning` as
    /// the model produces them. Returns the updated session.
    pub async fn send_streaming(
        &self,
        session_id: &str,
        text: &str,
        on_delta: &(dyn for<'a> Fn(&'a str) + Send + Sync),
        on_reasoning: &(dyn for<'a> Fn(&'a str) + Send + Sync),
    ) -> anyhow::Result<ChatSession> {
        {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(session_id)
                .ok_or_else(|| anyhow::anyhow!("no chat session '{session_id}'"))?;

            session
                .messages
                .push(ChatMessage::new(MessageRole::User, text.to_string()));

            if let Err(e) = session
                .runtime
                .run_turn_streaming(
                    &mut session.messages,
                    runtime::DEFAULT_MAX_ITERATIONS,
                    on_delta,
                    on_reasoning,
                )
                .await
            {
                session.messages.push(ChatMessage::new(
                    MessageRole::Assistant,
                    format!("(error: {e})"),
                ));
            }

            // Persist the updated conversation.
            db::save_chat_messages(&self.db, &session.conversation_id, &session.messages).await?;
        }

        Ok(self.session_view(session_id).await)
    }

    /// Fetch a session's current conversation.
    pub async fn messages(&self, session_id: &str) -> anyhow::Result<ChatSession> {
        Ok(self.session_view(session_id).await)
    }

    async fn session_view(&self, session_id: &str) -> ChatSession {
        let sessions = self.sessions.lock().await;
        match sessions.get(session_id) {
            Some(s) => ChatSession {
                id: s.id.clone(),
                task_id: s.task_id.clone(),
                messages: s.messages.clone(),
            },
            None => ChatSession {
                id: session_id.to_string(),
                task_id: None,
                messages: Vec::new(),
            },
        }
    }
}

/// Pull the human-readable output text out of a task's `output` JSON
/// (`{"output": "..."}`), falling back to the raw value.
fn extract_output(value: &serde_json::Value) -> Option<String> {
    value
        .get("output")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            let s = value.to_string();
            (!s.is_empty()).then_some(s)
        })
}
