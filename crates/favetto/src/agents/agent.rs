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

use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;

use favetto_core::model::{AgentCapabilities, AwaitingInputReason};

use super::state::{StateSource, StateSourceConfig};

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
    /// Whether the executable was found on the daemon's PATH. The registry
    /// overwrites this after construction via [`Agent::set_available`].
    pub available: bool,
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

/// Largest single argv element we hand to `execve`.
///
/// Linux caps one argument at `MAX_ARG_STRLEN` (128 KiB on 4 KiB pages). When a
/// prompt is passed as a single argument and exceeds it, the child's `execve`
/// fails with `E2BIG`; `portable-pty`'s pre-exec `close_random_fds` has already
/// closed the error-reporting pipe, so the failure surfaces as the opaque
/// `fatal runtime error: assertion failed: output.write(&bytes).is_ok()`
/// abort. Reject it up front with an actionable message instead.
pub(crate) const MAX_ARG_BYTES: usize = 120 * 1024;

/// Refuse to launch `program` when an argument cannot fit in one `execve` string.
pub(crate) fn check_arg_sizes(program: &Path, args: &[String]) -> anyhow::Result<()> {
    for arg in args {
        if arg.len() > MAX_ARG_BYTES {
            anyhow::bail!(
                "refusing to launch {}: an argument is {} bytes, over the {} byte \
                 command-line limit; the task prompt is too large to pass on the \
                 command line (bound the data rendered into it, e.g. `{{{{ prev.tasks }}}}`)",
                program.display(),
                arg.len(),
                MAX_ARG_BYTES
            );
        }
    }
    Ok(())
}

/// Where to read an agent's own session id from its line-delimited JSON output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionIdProbe {
    /// A top-level key (e.g. opencode's `"sessionID"`).
    JsonKey(String),
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

    /// Record whether the executable was found on PATH. Implementations that own
    /// an `AgentDescriptor` override this; the default is a no-op.
    fn set_available(&mut self, _available: bool) {}

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

    /// Whether [`Self::session_title`] can ever return a title. Defaults to
    /// `false`, so callers can skip the retry/backfill loop entirely for agents
    /// (claude, pi, vibe, custom) that never have titles.
    fn has_session_titles(&self) -> bool {
        false
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

    /// Build the live-state source for a launch, if this agent has one.
    ///
    /// Defaults to `None`: the session then relies on the debounced screen
    /// heuristic. An implementation that returns a source opts its sessions into
    /// structured state (and, where supported, structured input replies).
    fn state_source(&self, _cfg: &StateSourceConfig) -> Option<Box<dyn StateSource>> {
        None
    }

    /// Detect that the session's *visible screen* is blocked waiting on the user
    /// (a permission dialog, confirmation, choice, …). Defaults to `None`; the
    /// generic fallback in `AgentManager` still applies.
    fn awaiting_input(&self, _screen: &vt100::Screen) -> Option<AwaitingInputReason> {
        None
    }

    /// Whether a user-started interactive run's seeded turn has already finished,
    /// without waiting for the TUI process to exit.
    ///
    /// Some interactive CLIs (notably opencode) keep their TUI alive after the
    /// agent answers the prompt, so a task would otherwise stay `running` forever
    /// and never fire its `spawn`/`needs` successors. An implementation asks the
    /// CLI's own session store about the most recent session created in `cwd` at
    /// or after `since`:
    ///
    /// - `Some(true)` — the turn finished successfully;
    /// - `Some(false)` — the turn finished with a failure;
    /// - `None` — no signal, or the turn is still in flight (keep waiting).
    ///
    /// The default returns `None`, so an agent whose interactive mode exits on
    /// its own keeps the exit-based lifecycle. This is a blocking probe: callers
    /// run it off the async runtime.
    fn interactive_turn_done(&self, _cwd: &Path, _since: DateTime<Utc>) -> Option<bool> {
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
    let SessionIdProbe::JsonKey(key) = probe;
    value.get(key)?.as_str().map(str::to_string)
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

/// Poll policy for [`resolve_session_title`]: opencode writes the title a few
/// seconds after the first turn, so poll once a second for up to ~20 s.
pub(crate) const TITLE_POLL_ATTEMPTS: u32 = 20;
pub(crate) const TITLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);

/// Resolve an agent session's title, re-polling while the CLI may still be
/// generating it. Returns `None` immediately for agents without a title
/// source, and `None` after `attempts` polls if no title appears. The blocking
/// CLI lookup runs off the async runtime.
pub(crate) async fn resolve_session_title(
    agent: Arc<dyn Agent>,
    session_id: &str,
    cwd: &Path,
    attempts: u32,
    interval: std::time::Duration,
) -> Option<String> {
    if !agent.has_session_titles() || session_id.is_empty() {
        return None;
    }
    let attempts = attempts.max(1);
    for attempt in 0..attempts {
        let agent = agent.clone();
        let sid = session_id.to_string();
        let cwd = cwd.to_path_buf();
        let title = tokio::task::spawn_blocking(move || agent.session_title(&sid, &cwd))
            .await
            .ok()
            .flatten()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        if title.is_some() {
            return title;
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(interval).await;
        }
    }
    None
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
    fn check_arg_sizes_rejects_an_oversized_argument() {
        assert!(check_arg_sizes(Path::new("agent"), &["a".to_string()]).is_ok());

        let big = "x".repeat(MAX_ARG_BYTES + 1);
        let err = check_arg_sizes(Path::new("opencode"), &["run".to_string(), big]).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("opencode"), "error: {message}");
        assert!(message.contains("command-line limit"), "error: {message}");
    }

    #[test]
    fn extract_session_id_reads_top_level_key() {
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
            extract_session_id(&value, &SessionIdProbe::JsonKey("missing".to_string())),
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
        assert!(!Dummy.has_session_titles());

        // The default awaiting-input detector never fires.
        let parser = vt100::Parser::new(4, 20, 0);
        assert!(Dummy.awaiting_input(parser.screen()).is_none());

        // The default has no structured state source (screen fallback).
        assert!(Dummy.state_source(&StateSourceConfig::default()).is_none());
    }

    /// A title-capable fixture: returns `"Late title"` once `fail_first` lookups
    /// have already failed, counting every lookup.
    struct TitleAgent {
        descriptor: AgentDescriptor,
        calls: std::sync::atomic::AtomicU32,
        fail_first: u32,
        has_titles: bool,
    }

    impl Agent for TitleAgent {
        fn descriptor(&self) -> &AgentDescriptor {
            &self.descriptor
        }

        fn command(&self, _: &Invocation<'_>, _: &AgentContext) -> anyhow::Result<CommandSpec> {
            unimplemented!()
        }

        fn has_session_titles(&self) -> bool {
            self.has_titles
        }

        fn session_title(&self, _sid: &str, _cwd: &Path) -> Option<String> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (n >= self.fail_first).then(|| "Late title".to_string())
        }
    }

    fn title_agent(has_titles: bool, fail_first: u32) -> Arc<TitleAgent> {
        Arc::new(TitleAgent {
            descriptor: AgentDescriptor {
                id: "title".to_string(),
                name: "Title".to_string(),
                command: "true".to_string(),
                available: true,
                capabilities: AgentCapabilities::default(),
            },
            calls: std::sync::atomic::AtomicU32::new(0),
            fail_first,
            has_titles,
        })
    }

    #[tokio::test]
    async fn resolve_session_title_retries_until_available() {
        let agent = title_agent(true, 2);
        let title = resolve_session_title(
            agent.clone(),
            "ses_1",
            Path::new("/tmp"),
            5,
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(title.as_deref(), Some("Late title"));
        assert_eq!(
            agent.calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "should stop polling as soon as a title appears"
        );
    }

    #[tokio::test]
    async fn resolve_session_title_gives_up_after_attempts() {
        let agent = title_agent(true, 99);
        let title = resolve_session_title(
            agent.clone(),
            "ses_1",
            Path::new("/tmp"),
            3,
            Duration::from_millis(1),
        )
        .await;
        assert!(title.is_none());
        assert_eq!(agent.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn resolve_session_title_skips_agents_without_titles() {
        let agent = title_agent(false, 0);
        let title = resolve_session_title(
            agent.clone(),
            "ses_1",
            Path::new("/tmp"),
            5,
            Duration::from_millis(1),
        )
        .await;
        assert!(title.is_none());
        assert_eq!(agent.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
