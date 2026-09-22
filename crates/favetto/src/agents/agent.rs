//! The generic external-agent abstraction.
//!
//! favetto never implements an agent loop itself: it launches a configured CLI
//! and drives it. [`Agent`] is the seam that keeps the PTY/session machinery in
//! [`super::AgentManager`] free of CLI-specific branches. Each concrete CLI
//! (opencode, Claude, pi, Mistral Vibe, …) implements the trait with its own
//! launch spec, session-id probe, output parser, and provider catalog; a
//! template-only agent is served by
//! [`ConfigurableAgent`](super::configurable::ConfigurableAgent).

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;

use favetto_core::model::AgentCapabilities;

/// Delay before the first submit-Enter, and the maximum number of sends.
pub(crate) const SUBMIT_DELAY: Duration = Duration::from_millis(1200);
pub(crate) const SUBMIT_MAX_SENDS: u32 = 8;

/// Identity and resolved executable of an agent implementation.
#[derive(Debug, Clone)]
pub struct AgentDescriptor {
    /// Configured/built-in key, e.g. `opencode`.
    pub id: String,
    /// Human-readable CLI name, e.g. `OpenCode`.
    pub name: String,
    /// Resolved executable.
    pub command: String,
    /// What the agent can do.
    pub capabilities: AgentCapabilities,
}

/// Per-launch context: where and with which model the agent is started.
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    pub cwd: Option<PathBuf>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub session_id: Option<String>,
    pub cols: u16,
    pub rows: u16,
    /// Per-task override of the global/agent `[git] signing` mode. `None` uses
    /// the agent's configured (or global default) settings.
    pub git_signing: Option<crate::config::GitSigning>,
}

/// A concrete command to spawn in the PTY.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    /// Prompt to write to stdin when it was not placed on the command line.
    pub stdin_prompt: Option<String>,
    /// Send EOT after the stdin prompt so a headless run sees EOF.
    pub stdin_eof: bool,
    /// How to submit a prompt that was only pre-filled on the command line.
    pub submit: SubmitStrategy,
}

/// How (and whether) to press Enter to submit a pre-filled prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitStrategy {
    None,
    AfterSettle { delay: Duration, max_sends: u32 },
}

/// Where to read an agent's own session id from its line-delimited JSON output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionIdProbe {
    /// A top-level key (e.g. opencode's `"sessionID"`).
    JsonKey(String),
    /// A nested path of object keys. No shipped built-in needs this yet, but the
    /// trait is designed to grow; it is exercised by the test suite.
    #[allow(dead_code)]
    JsonPath(Vec<String>),
}

/// The outcome of a headless run.
#[derive(Debug, Clone)]
pub struct AgentRunResult {
    pub exit_code: Option<i32>,
    /// The agent's own session id, when the output carried one.
    pub session_id: Option<String>,
    /// Parsed output (`{ "text": raw }` unless the impl understands the format).
    pub output: serde_json::Value,
    /// Raw captured output.
    pub raw: String,
}

/// A source of provider/model information for an agent's CLI.
pub trait ProviderSource: Send + Sync {
    /// Provider ids the operator has configured/authenticated.
    fn configured_providers(&self) -> anyhow::Result<Vec<String>>;

    /// Fetch the full provider/model catalog.
    fn fetch<'a>(
        &'a self,
        client: &'a reqwest::Client,
    ) -> BoxFuture<'a, anyhow::Result<Vec<favetto_providers::Provider>>>;
}

/// How an agent is invoked for a session.
///
/// The variant selects the mode; the referenced values are the per-launch
/// provider/model/prompt. [`AgentContext`] carries the same values owned for
/// implementations that prefer to read them there.
pub enum Invocation<'a> {
    /// Interactive TUI: base `args` (or `interactive_model_args` when a model is
    /// given) plus `prompt_args`, or the prompt on stdin.
    Interactive {
        prompt: Option<&'a str>,
        provider: Option<&'a str>,
        model: Option<&'a str>,
    },
    /// Unattended run: `run_args` when a model is given, else `headless_args`.
    Headless {
        prompt: &'a str,
        provider: Option<&'a str>,
        model: Option<&'a str>,
    },
    /// Interactive reattach to an existing agent session (`resume_args`).
    Resume(&'a str),
}

/// A configurable external-agent CLI.
pub trait Agent: Send + Sync {
    /// Identity and resolved executable.
    fn descriptor(&self) -> &AgentDescriptor;

    /// What the agent can do (shorthand for `descriptor().capabilities`).
    fn capabilities(&self) -> AgentCapabilities {
        self.descriptor().capabilities
    }

    /// Build the command to spawn for `invocation`.
    fn command(
        &self,
        invocation: &Invocation<'_>,
        ctx: &AgentContext,
    ) -> anyhow::Result<CommandSpec>;

    /// Where to read the agent's session id from its output, if it reports one.
    fn session_id_probe(&self) -> Option<SessionIdProbe> {
        None
    }

    /// Look up the human-readable title of one of this agent's sessions, if its
    /// CLI exposes one. `cwd` is the directory the session ran in, for CLIs that
    /// scope their session store per project. The default is "no title".
    fn session_title(&self, _session_id: &str, _cwd: &Path) -> Option<String> {
        None
    }

    /// Parse a finished run's raw output.
    fn parse_output(&self, raw: &str, exit_code: Option<i32>) -> AgentRunResult {
        AgentRunResult {
            exit_code,
            session_id: None,
            output: serde_json::json!({ "text": raw }),
            raw: raw.to_string(),
        }
    }

    /// The agent's provider/model catalog source, if it has one.
    fn provider_source(&self) -> Option<Arc<dyn ProviderSource>> {
        None
    }
}

/// Substitute `{key}` placeholders in an argument template.
pub(crate) fn substitute(template: &[String], vars: &[(&str, &str)]) -> Vec<String> {
    template
        .iter()
        .map(|a| {
            let mut s = a.clone();
            for (k, v) in vars {
                s = s.replace(k, v);
            }
            s
        })
        .collect()
}

/// Append a rendered argument template, reporting whether it carried `{prompt}`.
pub(crate) fn append_template(
    args: &mut Vec<String>,
    template: &[String],
    vars: &[(&str, &str)],
) -> bool {
    let saw_prompt = template.iter().any(|a| a.contains("{prompt}"));
    args.extend(substitute(template, vars));
    saw_prompt
}

/// Read a session id from a parsed JSON value using `probe`.
pub(crate) fn extract_session_id(
    value: &serde_json::Value,
    probe: &SessionIdProbe,
) -> Option<String> {
    match probe {
        SessionIdProbe::JsonKey(key) => value.get(key)?.as_str().map(str::to_string),
        SessionIdProbe::JsonPath(path) => {
            let mut cursor = value;
            for segment in path {
                cursor = cursor.get(segment)?;
            }
            cursor.as_str().map(str::to_string)
        }
    }
}

/// Extract an agent session id from one line of line-delimited JSON output.
pub(crate) fn extract_session_id_from_line(line: &str, probe: &SessionIdProbe) -> Option<String> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    extract_session_id(&value, probe)
}

/// Extract the first session id found across line-delimited JSON output.
pub(crate) fn extract_session_id_from_lines(raw: &str, probe: &SessionIdProbe) -> Option<String> {
    raw.lines()
        .find_map(|line| extract_session_id_from_line(line, probe))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_renders_placeholders() {
        let template = vec![
            "run".to_string(),
            "--model".to_string(),
            "{provider}/{model}".to_string(),
        ];
        let rendered = substitute(&template, &[("{provider}", "jev"), ("{model}", "1.13")]);
        assert_eq!(rendered, vec!["run", "--model", "jev/1.13"]);
    }

    #[test]
    fn append_template_detects_prompt_placeholder() {
        let mut args = vec!["base".to_string()];
        let template = vec!["--prompt".to_string(), "{prompt}".to_string()];
        assert!(append_template(&mut args, &template, &[("{prompt}", "hi")]));
        assert_eq!(args, vec!["base", "--prompt", "hi"]);

        let mut args = Vec::new();
        assert!(!append_template(&mut args, &["--auto".to_string()], &[]));
    }

    #[test]
    fn extract_session_id_handles_json_key_and_path() {
        let value = serde_json::json!({
            "sessionID": "ses_top",
            "part": { "session": { "id": "ses_nested" } },
        });
        assert_eq!(
            extract_session_id(&value, &SessionIdProbe::JsonKey("sessionID".to_string()))
                .as_deref(),
            Some("ses_top")
        );
        assert_eq!(
            extract_session_id(
                &value,
                &SessionIdProbe::JsonPath(vec![
                    "part".to_string(),
                    "session".to_string(),
                    "id".to_string()
                ])
            )
            .as_deref(),
            Some("ses_nested")
        );
        assert_eq!(
            extract_session_id(&value, &SessionIdProbe::JsonKey("missing".to_string())),
            None
        );
        assert_eq!(
            extract_session_id(
                &value,
                &SessionIdProbe::JsonPath(vec!["part".to_string(), "missing".to_string()])
            ),
            None
        );
    }

    #[test]
    fn extract_session_id_from_line_rejects_non_json() {
        assert_eq!(
            extract_session_id_from_line("not json", &SessionIdProbe::JsonKey("id".to_string())),
            None
        );
        assert_eq!(
            extract_session_id_from_line(
                r#"{"id":"ses_abc"}"#,
                &SessionIdProbe::JsonKey("id".to_string())
            )
            .as_deref(),
            Some("ses_abc")
        );
    }

    #[test]
    fn default_parse_output_wraps_text() {
        struct Dummy;
        impl Agent for Dummy {
            fn descriptor(&self) -> &AgentDescriptor {
                unimplemented!()
            }
            fn command(&self, _: &Invocation<'_>, _: &AgentContext) -> anyhow::Result<CommandSpec> {
                unimplemented!()
            }
        }
        let result = Dummy.parse_output("hello", Some(0));
        assert_eq!(result.output, serde_json::json!({ "text": "hello" }));
        assert_eq!(result.raw, "hello");
        assert_eq!(result.exit_code, Some(0));
        assert!(result.session_id.is_none());

        // The default title lookup is always "no title".
        assert!(Dummy
            .session_title("ses_1", std::path::Path::new("/tmp"))
            .is_none());
    }
}
