//! Interactive LLM chat sessions, opened per-task from the TUI.
//!
//! A session holds a conversation (`Vec<ChatMessage>`) plus a [`ModelBackend`].
//! Sessions are currently in-memory (persistence arrives with the scheduler in M5);
//! the conversation is seeded from the task's skill prompt and input so the agent
//! has context about the task it is discussing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;
use uuid::Uuid;

use favetto_core::model::{ChatMessage, ChatSession, MessageRole};

use crate::config::FavettoConfig;
use crate::db;
use crate::runtime::{self, ModelBackend, ModelResponse};
use crate::skills;

/// Owns all live chat sessions, keyed by session id.
pub struct ChatManager {
    skills_dir: PathBuf,
    config: Arc<FavettoConfig>,
    sessions: Mutex<HashMap<String, ChatSessionState>>,
}

struct ChatSessionState {
    id: String,
    task_id: Option<String>,
    messages: Vec<ChatMessage>,
    backend: Arc<dyn ModelBackend>,
}

impl ChatManager {
    pub fn new(skills_dir: PathBuf, config: Arc<FavettoConfig>) -> Self {
        Self {
            skills_dir,
            config,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Open a session for a task: load the task + its skill, seed the conversation
    /// with the skill prompt and task input, and return the session.
    pub async fn open(
        &self,
        pool: &sqlx::SqlitePool,
        task_id: Option<String>,
    ) -> anyhow::Result<ChatSession> {
        let id = Uuid::new_v4().to_string();

        const DEFAULT_PROMPT: &str =
            "You are the favetto's agent. Help the user work on their task.\n";
        let mut system_prompt = DEFAULT_PROMPT.to_string();
        let mut user_prompt = "Hello!".to_string();
        let mut skill_name: Option<String> = None;

        if let Some(tid) = &task_id {
            if let Ok(u) = Uuid::parse_str(tid) {
                if let Ok(Some(task)) = db::get_task(pool, u).await {
                    system_prompt = task_skill_prompt(&self.skills_dir, &task.skill)
                        .await
                        .unwrap_or_else(|| DEFAULT_PROMPT.to_string());
                    user_prompt = format!("Task: {} ({})", task.skill, task.id);
                    skill_name = Some(task.skill);
                }
            }
        }

        let skill = match skill_name.as_deref() {
            Some(name) => skills::load_skills(&self.skills_dir)
                .ok()
                .and_then(|skills| skills.into_iter().find(|s| s.name == name)),
            None => None,
        };
        let backend = runtime::build_chat_backend(skill.as_ref(), &self.config);

        let messages = vec![
            ChatMessage::new(MessageRole::System, system_prompt),
            ChatMessage::new(MessageRole::User, user_prompt),
        ];

        self.sessions.lock().await.insert(
            id.clone(),
            ChatSessionState {
                id: id.clone(),
                task_id,
                messages,
                backend,
            },
        );

        Ok(self.session_view(&id).await)
    }

    /// Send a user message and run one agent turn, returning the updated session.
    pub async fn send(&self, session_id: &str, text: &str) -> anyhow::Result<ChatSession> {
        {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(session_id)
                .ok_or_else(|| anyhow::anyhow!("no chat session '{session_id}'"))?;

            session
                .messages
                .push(ChatMessage::new(MessageRole::User, text.to_string()));

            match session.backend.next(&session.messages).await? {
                ModelResponse::Final { text } => {
                    session
                        .messages
                        .push(ChatMessage::new(MessageRole::Assistant, text));
                }
                ModelResponse::ToolCall {
                    server,
                    tool,
                    arguments,
                } => {
                    // Text-only chat for now: record the tool call as a note.
                    session.messages.push(ChatMessage::new(
                        MessageRole::Assistant,
                        format!("[tool call {server}.{tool}({arguments}) — not available in chat yet]"),
                    ));
                }
            }
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

/// Read the skill prompt (AGENT.md) for a task's skill, if available.
async fn task_skill_prompt(skills_dir: &std::path::Path, skill_name: &str) -> Option<String> {
    let path = skills_dir.join(skill_name).join("AGENT.md");
    tokio::fs::read_to_string(path).await.ok()
}
