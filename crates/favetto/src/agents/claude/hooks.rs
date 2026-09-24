//! Claude Code hook transport: per-launch settings injection and payload mapping.
//!
//! Claude can POST lifecycle events (`UserPromptSubmit`, `PreToolUse`,
//! `PermissionRequest`, `Stop`, …) to an HTTP endpoint registered in a per-launch
//! `--settings` file. This module writes that ephemeral file (never the user or
//! project settings) and maps each hook payload to normalized
//! [`AgentStateEvent`]s. Interactive sessions are **observe-only**: the receiver
//! never returns a permission decision, and a session whose endpoint is blocked
//! keeps the screen heuristic.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use favetto_core::model::{
    AgentStateEvent, AwaitingInputKind, IdleOutcome, InputReply, InputRequest,
};

use crate::agent_hooks::{AgentHookLaunch, HookInjection, HookRouter};
use crate::agents::state::{
    InputResponder, StateContext, StateSource, StateSourceConfig, StateStart, StateStop,
    StdoutParser,
};

use super::stream_json::ClaudeStreamJsonParser;

/// Env var carrying the per-launch hook URL (including the token).
pub(crate) const HOOK_URL_ENV: &str = "FAVETTO_AGENT_HOOK_URL";
/// Env var carrying the path of the ephemeral settings file, for cleanup.
pub(crate) const HOOK_SETTINGS_ENV: &str = "FAVETTO_AGENT_HOOK_SETTINGS";

/// Build the per-launch hook injection: write a random-token settings file and
/// return the `--settings` argument plus the two discovery env vars.
///
/// Any I/O error degrades to the screen fallback (a warning, `None`) rather than
/// failing the launch.
pub(crate) fn injection(launch: &AgentHookLaunch) -> Option<HookInjection> {
    let token = uuid::Uuid::new_v4().to_string();
    let path = launch.dir.join(format!("{token}.json"));
    let url = format!("{}/{}", launch.endpoint.trim_end_matches('/'), token);
    let settings = settings_json(&url);
    if let Err(e) = write_settings(&path, &settings) {
        tracing::warn!(
            error = %e,
            path = %path.display(),
            "failed to write claude hook settings; falling back to screen state"
        );
        return None;
    }
    let mut env = BTreeMap::new();
    env.insert(HOOK_URL_ENV.to_string(), url);
    env.insert(
        HOOK_SETTINGS_ENV.to_string(),
        path.to_string_lossy().to_string(),
    );
    Some(HookInjection {
        args: vec!["--settings".to_string(), path.to_string_lossy().to_string()],
        env,
    })
}

fn write_settings(path: &Path, settings: &serde_json::Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec(settings).map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

/// The `--settings` document: `type: "http"` handlers only, no `SessionStart`
/// (Claude only supports `command`/`mcp_tool` there).
fn settings_json(url: &str) -> serde_json::Value {
    serde_json::json!({
        "hooks": {
            "UserPromptSubmit": [ { "hooks": [ { "type": "http", "url": url } ] } ],
            "PreToolUse": [ { "hooks": [ { "type": "http", "url": url } ] } ],
            "PostToolUse": [ { "hooks": [ { "type": "http", "url": url } ] } ],
            "PostToolUseFailure": [ { "hooks": [ { "type": "http", "url": url } ] } ],
            "PermissionRequest": [ { "hooks": [ { "type": "http", "url": url } ] } ],
            "Notification": [ {
                "matcher": "permission_prompt|idle_prompt|agent_needs_input",
                "hooks": [ { "type": "http", "url": url } ],
            } ],
            "Stop": [ { "hooks": [ { "type": "http", "url": url } ] } ],
            "StopFailure": [ { "hooks": [ { "type": "http", "url": url } ] } ],
            "SubagentStart": [ { "hooks": [ { "type": "http", "url": url } ] } ],
            "SubagentStop": [ { "hooks": [ { "type": "http", "url": url } ] } ],
            "SessionEnd": [ { "hooks": [ { "type": "http", "url": url } ] } ],
        }
    })
}

/// Derive the per-launch token from the final path segment of a hook URL.
fn token_from_url(url: &str) -> Option<String> {
    url.rsplit('/')
        .next()
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// The live-state source for a claude session: an optional stdout parser
/// (headless) plus the hook receiver (when a launch registered a token).
pub(crate) struct ClaudeHookSource {
    router: Option<Arc<HookRouter>>,
    token: Option<String>,
    hook_url: Option<String>,
    settings_path: Option<PathBuf>,
    /// The headless stdout parser, taken on `start` (sources are single-use).
    stdout: Mutex<Option<Box<dyn StdoutParser>>>,
}

impl ClaudeHookSource {
    fn new(cfg: &StateSourceConfig, stdout: Option<Box<dyn StdoutParser>>) -> Self {
        let hook_url = cfg.env.get(HOOK_URL_ENV).cloned();
        let token = hook_url.as_deref().and_then(token_from_url);
        let settings_path = cfg.env.get(HOOK_SETTINGS_ENV).map(PathBuf::from);
        let router = cfg.hook_router.clone().filter(|_| token.is_some());
        Self {
            router,
            token,
            hook_url,
            settings_path,
            stdout: Mutex::new(stdout),
        }
    }

    fn hooks_enabled(&self) -> bool {
        self.router.is_some() && self.token.is_some()
    }
}

impl StateSource for ClaudeHookSource {
    fn label(&self) -> &'static str {
        if self.token.is_some() {
            "claude-hooks"
        } else {
            "claude-stream-json"
        }
    }

    fn start(&self, _ctx: StateContext) -> anyhow::Result<StateStart> {
        let (tx, events) = mpsc::unbounded_channel();
        let stop = if let (Some(router), Some(token)) = (self.router.clone(), self.token.clone()) {
            if let Some(url) = &self.hook_url {
                tracing::debug!(url, "claude hook source started");
            }
            let mut mapper = HookMapper::new();
            let registration = router.register(token, tx, Box::new(move |value| mapper.map(value)));
            let settings_path = self.settings_path.clone();
            Some(Box::new(move || {
                drop(registration);
                if let Some(path) = settings_path {
                    let _ = std::fs::remove_file(path);
                }
            }) as StateStop)
        } else {
            None
        };
        Ok(StateStart {
            events,
            stdout: self.stdout.lock().take(),
            responder: Arc::new(ObserveOnlyResponder),
            stop,
        })
    }
}

/// Build the state source for a claude launch.
///
/// Headless runs always get the stdout parser; hooks are added when the launch
/// registered a token. Interactive sessions get a hook source only when hooks
/// are available, else `None` (the debounced screen heuristic).
pub(crate) fn state_source(cfg: &StateSourceConfig) -> Option<Box<dyn StateSource>> {
    if cfg.headless {
        let stdout: Box<dyn StdoutParser> = Box::new(ClaudeStreamJsonParser::new());
        Some(Box::new(ClaudeHookSource::new(cfg, Some(stdout))))
    } else {
        let source = ClaudeHookSource::new(cfg, None);
        if source.hooks_enabled() {
            Some(Box::new(source))
        } else {
            None
        }
    }
}

/// A responder for observe-only sessions: never answers, so the user's real
/// dialog in the TUI stays authoritative.
struct ObserveOnlyResponder;

impl InputResponder for ObserveOnlyResponder {
    fn reply<'a>(
        &'a self,
        request_id: &'a str,
        _reply: InputReply,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        tracing::debug!(request_id, "claude hooks are observe-only; ignoring reply");
        Box::pin(async {
            Err(anyhow::anyhow!(
                "claude session is observe-only; answer in the terminal"
            ))
        })
    }
}

/// Maps one claude hook payload to normalized events for one session.
pub(crate) struct HookMapper {
    pending_permission: Option<String>,
    tool_names: HashMap<String, String>,
    announced_session: bool,
    finished: bool,
    seq: u64,
}

impl HookMapper {
    pub(crate) fn new() -> Self {
        Self {
            pending_permission: None,
            tool_names: HashMap::new(),
            announced_session: false,
            finished: false,
            seq: 0,
        }
    }

    pub(crate) fn map(&mut self, payload: &serde_json::Value) -> Vec<AgentStateEvent> {
        let mut events = Vec::new();
        self.announce_session(payload, &mut events);

        match str_field(payload, "hook_event_name").as_str() {
            "UserPromptSubmit" => events.push(AgentStateEvent::TurnStarted),
            "PreToolUse" => {
                if let Some(id) = self.pending_permission.take() {
                    events.push(AgentStateEvent::InputResolved { id });
                }
                let id = str_field(payload, "tool_use_id");
                let name = str_field(payload, "tool_name");
                let input = payload
                    .get("tool_input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                self.tool_names.insert(id.clone(), name.clone());
                events.push(AgentStateEvent::ToolStarted { id, name, input });
            }
            "PostToolUse" => events.push(self.tool_finished(payload, true)),
            "PostToolUseFailure" => events.push(self.tool_finished(payload, false)),
            "PermissionRequest" => {
                let id = format!("claude-perm-{}", self.next_seq());
                let message = permission_message(payload);
                let options = permission_options(payload);
                self.pending_permission = Some(id.clone());
                events.push(AgentStateEvent::InputRequested {
                    request: InputRequest {
                        id,
                        kind: AwaitingInputKind::Permission,
                        message,
                        options,
                        allow_always: true,
                    },
                });
            }
            "Notification" => self.map_notification(payload, &mut events),
            "Stop" => {
                if let Some(id) = self.pending_permission.take() {
                    events.push(AgentStateEvent::InputResolved { id });
                }
                if let Some(idle) = self.idle(IdleOutcome::Succeeded) {
                    events.push(idle);
                }
            }
            "StopFailure" => {
                if let Some(idle) = self.idle(IdleOutcome::Failed) {
                    events.push(idle);
                }
            }
            "SubagentStart" => {
                let id = str_field(payload, "agent_id");
                let name = str_field(payload, "agent_type");
                self.tool_names.insert(id.clone(), name.clone());
                events.push(AgentStateEvent::ToolStarted {
                    id,
                    name,
                    input: serde_json::Value::Null,
                });
            }
            "SubagentStop" => {
                let id = str_field(payload, "agent_id");
                let name = self
                    .tool_names
                    .get(&id)
                    .filter(|name| !name.is_empty())
                    .cloned()
                    .unwrap_or_else(|| str_field(payload, "agent_type"));
                events.push(AgentStateEvent::ToolFinished {
                    id,
                    name,
                    ok: true,
                    output: None,
                });
            }
            "SessionEnd" => {
                if let Some(idle) = self.idle(IdleOutcome::Interrupted) {
                    events.push(idle);
                }
            }
            _ => {}
        }
        events
    }

    fn announce_session(&mut self, payload: &serde_json::Value, events: &mut Vec<AgentStateEvent>) {
        if self.announced_session {
            return;
        }
        let Some(session_id) = payload
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        self.announced_session = true;
        events.push(AgentStateEvent::Session {
            session_id: Some(session_id.to_string()),
            title: payload
                .get("session_title")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            model: None,
        });
    }

    fn map_notification(&mut self, payload: &serde_json::Value, events: &mut Vec<AgentStateEvent>) {
        match str_field(payload, "notification_type").as_str() {
            "permission_prompt" => {
                if self.pending_permission.is_some() {
                    return;
                }
                let id = format!("claude-perm-{}", self.next_seq());
                let message = str_field(payload, "message");
                self.pending_permission = Some(id.clone());
                events.push(AgentStateEvent::InputRequested {
                    request: InputRequest {
                        id,
                        kind: AwaitingInputKind::Permission,
                        message: default_message(message, "Permission requested"),
                        options: Vec::new(),
                        allow_always: true,
                    },
                });
            }
            "agent_needs_input" => {
                let id = format!("claude-input-{}", self.next_seq());
                let message = str_field(payload, "message");
                events.push(AgentStateEvent::InputRequested {
                    request: InputRequest {
                        id,
                        kind: AwaitingInputKind::Other,
                        message: default_message(message, "Agent needs input"),
                        options: Vec::new(),
                        allow_always: false,
                    },
                });
            }
            // `idle_prompt` and anything unknown are ignored.
            _ => {}
        }
    }

    fn tool_finished(&mut self, payload: &serde_json::Value, ok: bool) -> AgentStateEvent {
        let id = str_field(payload, "tool_use_id");
        let name = self
            .tool_names
            .get(&id)
            .filter(|name| !name.is_empty())
            .cloned()
            .unwrap_or_else(|| str_field(payload, "tool_name"));
        AgentStateEvent::ToolFinished {
            id,
            name,
            ok,
            output: payload.get("tool_response").cloned(),
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn idle(&mut self, outcome: IdleOutcome) -> Option<AgentStateEvent> {
        if self.finished {
            return None;
        }
        self.finished = true;
        Some(AgentStateEvent::Idle { outcome })
    }
}

fn str_field(payload: &serde_json::Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn default_message(message: String, fallback: &str) -> String {
    if message.is_empty() {
        fallback.to_string()
    } else {
        message
    }
}

fn permission_message(payload: &serde_json::Value) -> String {
    let tool = payload
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("a tool");
    format!("Claude requests permission to run {tool}")
}

fn permission_options(payload: &serde_json::Value) -> Vec<String> {
    let Some(suggestions) = payload
        .get("permission_suggestions")
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    suggestions
        .iter()
        .filter_map(|suggestion| {
            if let Some(text) = suggestion.as_str() {
                return Some(text.to_string());
            }
            suggestion
                .get("label")
                .and_then(|v| v.as_str())
                .or_else(|| suggestion.get("behavior").and_then(|v| v.as_str()))
                .map(str::to_string)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_launch() -> (AgentHookLaunch, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("favetto-claude-hooks-{}", uuid::Uuid::new_v4()));
        let launch = AgentHookLaunch {
            endpoint: "http://127.0.0.1:7878/agent-hooks/claude".to_string(),
            dir: dir.clone(),
        };
        (launch, dir)
    }

    #[test]
    fn settings_registers_http_hooks_only() {
        let url = "http://127.0.0.1:7878/agent-hooks/claude/tok123";
        let settings = settings_json(url);
        let hooks = settings["hooks"].as_object().expect("hooks object");

        assert!(
            !hooks.contains_key("SessionStart"),
            "SessionStart only supports command/mcp_tool handlers"
        );

        for (event, groups) in hooks {
            for group in groups.as_array().expect("groups") {
                for handler in group["hooks"].as_array().expect("handlers") {
                    assert_eq!(handler["type"], "http", "event {event}");
                    assert_eq!(handler["url"], url, "event {event}");
                    assert!(
                        handler.get("headers").is_none(),
                        "the token lives in the URL path, not headers"
                    );
                }
            }
        }

        assert_eq!(
            settings["hooks"]["Notification"][0]["matcher"],
            "permission_prompt|idle_prompt|agent_needs_input"
        );
    }

    #[test]
    fn injection_writes_settings_and_sets_env() {
        let (launch, dir) = temp_launch();
        let injection = injection(&launch).expect("injection");

        assert_eq!(injection.args[0], "--settings");
        let path = PathBuf::from(&injection.args[1]);
        assert!(path.starts_with(&dir));
        assert!(path.exists(), "settings file written to {}", path.display());
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(settings["hooks"]["PreToolUse"].is_array());

        let url = injection.env.get(HOOK_URL_ENV).expect("hook url env");
        assert!(url.starts_with("http://127.0.0.1:7878/agent-hooks/claude/"));
        assert_eq!(
            injection.env.get(HOOK_SETTINGS_ENV).map(String::as_str),
            Some(path.to_string_lossy().as_ref())
        );

        // Never the user or project settings.
        let rendered = path.to_string_lossy();
        assert!(!rendered.contains("/.claude/"), "path: {rendered}");
        assert!(!rendered.ends_with("/.claude/settings.json"));
        assert!(!rendered.ends_with("/.claude/settings.local.json"));
        assert!(rendered.ends_with(".json"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn injection_degrades_when_the_directory_cannot_be_created() {
        // A file where the directory should be makes `create_dir_all` fail; the
        // launch must fall back rather than error.
        let file = std::env::temp_dir().join(format!("favetto-hook-file-{}", uuid::Uuid::new_v4()));
        std::fs::write(&file, b"not a directory").unwrap();
        let launch = AgentHookLaunch {
            endpoint: "http://127.0.0.1:7878/agent-hooks/claude".to_string(),
            dir: file.join("nested"),
        };
        assert!(injection(&launch).is_none());
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn permission_request_maps_to_input_requested() {
        let mut mapper = HookMapper::new();
        let events = mapper.map(&serde_json::json!({
            "session_id": "ses_1",
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "permission_suggestions": [
                {"behavior": "allow", "label": "Allow once"},
                {"behavior": "deny", "label": "Reject"}
            ],
        }));
        assert_eq!(
            events,
            vec![
                AgentStateEvent::Session {
                    session_id: Some("ses_1".to_string()),
                    title: None,
                    model: None,
                },
                AgentStateEvent::InputRequested {
                    request: InputRequest {
                        id: "claude-perm-1".to_string(),
                        kind: AwaitingInputKind::Permission,
                        message: "Claude requests permission to run Bash".to_string(),
                        options: vec!["Allow once".to_string(), "Reject".to_string()],
                        allow_always: true,
                    },
                },
            ]
        );
    }

    #[test]
    fn stop_maps_to_idle_succeeded_and_clears_pending() {
        let mut mapper = HookMapper::new();
        mapper.map(&serde_json::json!({
            "session_id": "ses_1",
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
        }));
        // A `PreToolUse` resolves the pending prompt, then `Stop` goes idle.
        let events = mapper.map(&serde_json::json!({
            "session_id": "ses_1",
            "hook_event_name": "PreToolUse",
            "tool_use_id": "tu_1",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
        }));
        assert_eq!(
            events,
            vec![
                AgentStateEvent::InputResolved {
                    id: "claude-perm-1".to_string()
                },
                AgentStateEvent::ToolStarted {
                    id: "tu_1".to_string(),
                    name: "Bash".to_string(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ]
        );

        let events = mapper.map(&serde_json::json!({
            "session_id": "ses_1",
            "hook_event_name": "Stop",
        }));
        assert_eq!(
            events,
            vec![AgentStateEvent::Idle {
                outcome: IdleOutcome::Succeeded
            }]
        );
        // Idle is emitted once even if `SessionEnd` follows.
        let events = mapper.map(&serde_json::json!({
            "session_id": "ses_1",
            "hook_event_name": "SessionEnd",
        }));
        assert!(events.is_empty());
    }

    #[test]
    fn stop_failure_maps_to_failed() {
        let mut mapper = HookMapper::new();
        let events = mapper.map(&serde_json::json!({
            "session_id": "ses_1",
            "hook_event_name": "StopFailure",
        }));
        assert_eq!(
            events,
            vec![
                AgentStateEvent::Session {
                    session_id: Some("ses_1".to_string()),
                    title: None,
                    model: None,
                },
                AgentStateEvent::Idle {
                    outcome: IdleOutcome::Failed
                },
            ]
        );
    }

    #[test]
    fn pre_and_post_tool_use_map_to_tool_lifecycle() {
        let mut mapper = HookMapper::new();
        mapper.map(&serde_json::json!({
            "session_id": "ses_1",
            "hook_event_name": "PreToolUse",
            "tool_use_id": "tu_1",
            "tool_name": "Read",
            "tool_input": {"path": "a.txt"},
        }));
        let events = mapper.map(&serde_json::json!({
            "session_id": "ses_1",
            "hook_event_name": "PostToolUse",
            "tool_use_id": "tu_1",
            "tool_name": "Read",
            "tool_response": "contents",
        }));
        assert_eq!(
            events,
            vec![AgentStateEvent::ToolFinished {
                id: "tu_1".to_string(),
                name: "Read".to_string(),
                ok: true,
                output: Some(serde_json::json!("contents")),
            }]
        );

        let events = mapper.map(&serde_json::json!({
            "session_id": "ses_1",
            "hook_event_name": "PostToolUseFailure",
            "tool_use_id": "tu_2",
            "tool_name": "Bash",
        }));
        assert_eq!(
            events,
            vec![AgentStateEvent::ToolFinished {
                id: "tu_2".to_string(),
                name: "Bash".to_string(),
                ok: false,
                output: None,
            }]
        );
    }

    #[test]
    fn user_prompt_submit_emits_session_and_turn() {
        let mut mapper = HookMapper::new();
        let events = mapper.map(&serde_json::json!({
            "session_id": "ses_1",
            "session_title": "My session",
            "hook_event_name": "UserPromptSubmit",
        }));
        assert_eq!(
            events,
            vec![
                AgentStateEvent::Session {
                    session_id: Some("ses_1".to_string()),
                    title: Some("My session".to_string()),
                    model: None,
                },
                AgentStateEvent::TurnStarted,
            ]
        );
        // The session is announced only once.
        let events = mapper.map(&serde_json::json!({
            "session_id": "ses_1",
            "hook_event_name": "UserPromptSubmit",
        }));
        assert_eq!(events, vec![AgentStateEvent::TurnStarted]);
    }

    #[test]
    fn unknown_event_is_ignored() {
        let mut mapper = HookMapper::new();
        let events = mapper.map(&serde_json::json!({
            "session_id": "ses_1",
            "hook_event_name": "SomethingNew",
        }));
        assert_eq!(
            events,
            vec![AgentStateEvent::Session {
                session_id: Some("ses_1".to_string()),
                title: None,
                model: None,
            }]
        );
    }

    #[test]
    fn notification_permission_prompt_maps_to_input_requested() {
        let mut mapper = HookMapper::new();
        let events = mapper.map(&serde_json::json!({
            "session_id": "ses_1",
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
            "message": "Permission needed",
        }));
        assert_eq!(
            events,
            vec![
                AgentStateEvent::Session {
                    session_id: Some("ses_1".to_string()),
                    title: None,
                    model: None,
                },
                AgentStateEvent::InputRequested {
                    request: InputRequest {
                        id: "claude-perm-1".to_string(),
                        kind: AwaitingInputKind::Permission,
                        message: "Permission needed".to_string(),
                        options: Vec::new(),
                        allow_always: true,
                    },
                },
            ]
        );
    }

    #[test]
    fn state_source_selects_transports_by_mode() {
        use crate::agents::state::StateSourceConfig;

        // Interactive without hooks -> no source (screen fallback).
        let interactive = StateSourceConfig {
            headless: false,
            ..Default::default()
        };
        assert!(state_source(&interactive).is_none());
        assert_eq!(
            ClaudeStreamJsonParser::new().summary().outcome,
            None,
            "sanity: a fresh parser has no outcome"
        );

        // Headless always gets the stdout parser.
        let headless = StateSourceConfig {
            headless: true,
            ..Default::default()
        };
        let source = state_source(&headless).expect("headless source");
        assert_eq!(source.label(), "claude-stream-json");
    }
}
