//! Built-in agent for [pi](https://github.com/badlogic/pi-mono).
//!
//! Headless runs launch pi in its long-lived `--mode rpc` protocol and attach a
//! [`PiRpcSource`](rpc::PiRpcSource) state source, so a task reports its session
//! id, assistant text, tool calls, usage, and dialog prompts through the
//! structured `StateSource` seam. Interactive and resumed runs keep the bare TUI
//! and attach a bounded, read-only
//! [`PiSessionFileSource`](file_tail::PiSessionFileSource) that tails pi's own
//! session JSONL for best-effort activity, usage and cost; the debounced screen
//! fallback still covers awaiting-input.

use std::path::{Path, PathBuf};

use crate::config::AgentConfig;

use super::agent::{
    Agent, AgentContext, AgentDescriptor, AgentRunResult, CommandSpec, Invocation, SessionIdProbe,
    SubmitStrategy,
};
use super::configurable::{overlay, TemplateAgent};
use super::state::{StateSource, StateSourceConfig};

mod file_tail;
mod json;
mod rpc;
mod session_file;

/// The built-in defaults, matching `config.example.toml`.
fn base_config() -> AgentConfig {
    AgentConfig {
        command: "pi".to_string(),
        args: Vec::new(),
        // pi submits a positional initial message itself, so `submit_prompt`
        // stays unset (same as `claude`).
        prompt_args: Some(vec!["{prompt}".to_string()]),
        // Headless tasks speak the long-lived RPC protocol: the prompt is sent
        // over stdin by `rpc::PiRpcSource`, not on the command line, so a prompt
        // is no longer bounded by the `execve` argument limit. A deterministic
        // `{session_id}` is bound to every headless run so the created session
        // can later be reopened with `resume_args`.
        headless_args: Some(vec![
            "--mode".to_string(),
            "rpc".to_string(),
            "--session-id".to_string(),
            "{session_id}".to_string(),
        ]),
        run_args: Some(vec![
            "--mode".to_string(),
            "rpc".to_string(),
            "--provider".to_string(),
            "{provider}".to_string(),
            "--model".to_string(),
            "{model}".to_string(),
            "--session-id".to_string(),
            "{session_id}".to_string(),
        ]),
        resume_args: Some(vec!["--session".to_string(), "{session_id}".to_string()]),
        interactive_model_args: Some(vec![
            "--provider".to_string(),
            "{provider}".to_string(),
            "--model".to_string(),
            "{model}".to_string(),
        ]),
        ..Default::default()
    }
}

/// The pi implementation: interactive and headless (with or without a model).
pub struct PiAgent {
    template: TemplateAgent,
}

impl PiAgent {
    /// Build pi for the `[agents.<name>]` entry `overrides`.
    pub fn from_config(name: &str, overrides: &AgentConfig) -> Self {
        let mut config = base_config();
        overlay(&mut config, overrides);
        let mut template = TemplateAgent::new(name, "Pi", config);
        // Headless pi reports live state and answers dialogs over RPC
        // (`extension_ui_response`); interactive pi reports best-effort state
        // from its session file, with dialogs left to the TUI/screen fallback.
        template.descriptor.capabilities.reports_state = true;
        template.descriptor.capabilities.permission_channel = true;
        Self { template }
    }
}

impl Agent for PiAgent {
    fn descriptor(&self) -> &AgentDescriptor {
        &self.template.descriptor
    }

    fn set_available(&mut self, available: bool) {
        self.template.descriptor.available = available;
    }

    fn command(
        &self,
        invocation: &Invocation<'_>,
        ctx: &AgentContext,
    ) -> anyhow::Result<CommandSpec> {
        if let Invocation::Headless { .. } = invocation {
            let cfg = &self.template.config;
            if cfg.command.trim().is_empty() {
                anyhow::bail!(
                    "agent '{}' has no command configured",
                    self.template.descriptor.id
                );
            }
            // Build the RPC invocation explicitly: the prompt travels over the
            // RPC `prompt` command, never as a positional argument, and stdin
            // must stay open for the protocol (no `stdin_eof`).
            let mut args = vec!["--mode".to_string(), "rpc".to_string()];
            if let Some(model) = &ctx.model {
                args.push("--provider".to_string());
                args.push(ctx.provider.clone().unwrap_or_default());
                args.push("--model".to_string());
                args.push(model.clone());
            }
            if let Some(session_id) = &ctx.session_id {
                args.push("--session-id".to_string());
                args.push(session_id.clone());
            }
            return Ok(CommandSpec {
                program: PathBuf::from(&cfg.command),
                args,
                env: cfg.env.clone(),
                cwd: ctx.cwd.clone().or_else(|| cfg.cwd.clone()),
                stdin_prompt: None,
                stdin_eof: false,
                submit: SubmitStrategy::None,
            });
        }
        self.template.command(invocation, ctx)
    }

    fn session_id_probe(&self) -> Option<SessionIdProbe> {
        self.template.probe.clone()
    }

    fn state_source(&self, cfg: &StateSourceConfig) -> Option<Box<dyn StateSource>> {
        // A headless launch that resolved to `--mode rpc` speaks the structured
        // protocol; every other pi launch (interactive/resume) is observed
        // best-effort by tailing its session file, which needs the launch cwd.
        let is_rpc = cfg.headless
            && cfg
                .args
                .windows(2)
                .any(|pair| pair[0] == "--mode" && pair[1] == "rpc");
        if is_rpc {
            return Some(Box::new(rpc::PiRpcSource::new()) as Box<dyn StateSource>);
        }
        (!cfg.headless && cfg.cwd.is_some())
            .then(|| Box::new(file_tail::PiSessionFileSource::new()) as Box<dyn StateSource>)
    }

    fn parse_output(&self, raw: &str, exit_code: Option<i32>) -> AgentRunResult {
        let mut parser = json::PiJsonlParser::new();
        parser.push(raw.as_bytes());
        parser.finish(exit_code);
        let summary = parser.summary();
        AgentRunResult {
            exit_code,
            session_id: summary.session_id.clone(),
            output: serde_json::to_value(summary).unwrap_or(serde_json::Value::Null),
            raw: raw.to_string(),
        }
    }

    fn has_session_titles(&self) -> bool {
        true
    }

    fn session_title(&self, session_id: &str, cwd: &Path) -> Option<String> {
        session_file::find_session_file(cwd, session_id)
            .and_then(|path| session_file::read_session_name(&path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::agent::{AgentContext, Invocation, SubmitStrategy};
    use crate::agents::state::StateSourceConfig;

    fn ctx(prompt: Option<&str>, provider: Option<&str>, model: Option<&str>) -> AgentContext {
        AgentContext {
            prompt: prompt.map(str::to_string),
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
            // A headless run always carries a deterministic id.
            session_id: Some("ses-1234".to_string()),
            ..Default::default()
        }
    }

    fn agent() -> PiAgent {
        PiAgent::from_config("pi", &AgentConfig::default())
    }

    fn state_config(headless: bool, args: Vec<&str>) -> StateSourceConfig {
        StateSourceConfig {
            headless,
            args: args.into_iter().map(str::to_string).collect(),
            cwd: Some(PathBuf::from("/home/me/proj")),
            ..Default::default()
        }
    }

    #[test]
    fn capabilities_include_headless_model_state_and_permissions() {
        let caps = agent().capabilities();
        assert!(caps.interactive);
        assert!(caps.headless);
        assert!(caps.model_selection);
        assert!(caps.resume);
        assert!(!caps.providers);
        assert!(!caps.structured_output);
        assert!(!caps.reports_session_id);
        assert!(!caps.prompt_prefill);
        assert!(caps.reports_state);
        assert!(caps.permission_channel);
    }

    #[test]
    fn interactive_launches_bare_command() {
        let spec = agent()
            .command(
                &Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                &ctx(None, None, None),
            )
            .unwrap();
        assert_eq!(spec.program, std::path::PathBuf::from("pi"));
        assert!(spec.args.is_empty());
    }

    #[test]
    fn interactive_prompt_uses_prompt_args() {
        let spec = agent()
            .command(
                &Invocation::Interactive {
                    prompt: Some("hi"),
                    provider: None,
                    model: None,
                },
                &ctx(Some("hi"), None, None),
            )
            .unwrap();
        assert_eq!(spec.args, vec!["hi"]);
        assert!(spec.stdin_prompt.is_none());
        assert!(matches!(spec.submit, SubmitStrategy::None));
    }

    #[test]
    fn interactive_model_uses_provider_and_model() {
        let spec = agent()
            .command(
                &Invocation::Interactive {
                    prompt: None,
                    provider: Some("mistral"),
                    model: Some("large"),
                },
                &ctx(None, Some("mistral"), Some("large")),
            )
            .unwrap();
        assert_eq!(spec.args, vec!["--provider", "mistral", "--model", "large"]);
    }

    #[test]
    fn headless_without_model_uses_rpc_args() {
        let spec = agent()
            .command(
                &Invocation::Headless {
                    prompt: "do it",
                    provider: None,
                    model: None,
                },
                &ctx(Some("do it"), None, None),
            )
            .unwrap();
        assert_eq!(spec.args, vec!["--mode", "rpc", "--session-id", "ses-1234"]);
        // The prompt is sent over RPC, not written to stdin.
        assert!(spec.stdin_prompt.is_none());
        assert!(!spec.stdin_eof);
    }

    #[test]
    fn headless_with_model_uses_rpc_run_args() {
        let spec = agent()
            .command(
                &Invocation::Headless {
                    prompt: "do it",
                    provider: Some("mistral"),
                    model: Some("large"),
                },
                &ctx(Some("do it"), Some("mistral"), Some("large")),
            )
            .unwrap();
        assert_eq!(
            spec.args,
            vec![
                "--mode",
                "rpc",
                "--provider",
                "mistral",
                "--model",
                "large",
                "--session-id",
                "ses-1234"
            ]
        );
        assert!(spec.stdin_prompt.is_none());
        assert!(!spec.stdin_eof);
    }

    #[test]
    fn resume_reopens_a_session_interactively() {
        let spec = agent()
            .command(&Invocation::Resume("ses-1234"), &ctx(None, None, None))
            .unwrap();
        assert_eq!(spec.args, vec!["--session", "ses-1234"]);
        assert!(spec.stdin_prompt.is_none());
    }

    #[test]
    fn headless_rpc_uses_a_state_source() {
        let spec = agent()
            .command(
                &Invocation::Headless {
                    prompt: "do it",
                    provider: None,
                    model: None,
                },
                &ctx(Some("do it"), None, None),
            )
            .unwrap();
        let source = agent().state_source(&state_config(
            true,
            spec.args.iter().map(|s| s.as_str()).collect(),
        ));
        assert!(source.is_some());
    }

    #[test]
    fn interactive_uses_a_session_file_source() {
        // The interactive TUI has no RPC channel; its session file is tailed
        // when the launch working directory is known.
        assert!(agent().state_source(&state_config(false, vec![])).is_some());
        assert!(agent()
            .state_source(&state_config(false, vec!["--session", "ses-1234"]))
            .is_some());
        // Without a working directory there is nothing to tail: keep the screen
        // heuristic rather than pinning activity at "starting" forever.
        let no_cwd = StateSourceConfig {
            headless: false,
            ..Default::default()
        };
        assert!(agent().state_source(&no_cwd).is_none());
        // A headless launch that is not RPC (a user override) does too.
        assert!(agent()
            .state_source(&state_config(true, vec!["-p", "{prompt}"]))
            .is_none());
    }

    #[test]
    fn parse_output_summarizes_an_rpc_stream() {
        let raw = concat!(
            r#"{"type":"response","command":"get_state","success":true,"data":{"sessionId":"ses_pi"}}"#,
            "\n",
            r#"{"type":"agent_start"}"#,
            "\n",
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"hi"}}"#,
            "\n",
            r#"{"type":"agent_settled"}"#,
            "\n",
        );
        let result = agent().parse_output(raw, Some(0));
        assert_eq!(result.session_id.as_deref(), Some("ses_pi"));
        assert_eq!(result.output["text"], "hi");
        assert_eq!(result.output["outcome"], "succeeded");
        assert_eq!(result.raw, raw);
    }

    #[test]
    fn session_title_reads_the_session_file() {
        let root = std::env::temp_dir().join(format!(
            "favetto-pi-title-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let cwd = std::path::Path::new("/home/me/proj");
        let dir = root
            .join("agent")
            .join("sessions")
            .join(format!("--{}--", session_file::cwd_slug(cwd)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("111_ses-1234.jsonl"),
            concat!(
                r#"{"type":"session","version":3,"id":"ses-1234","cwd":"/home/me/proj"}"#,
                "\n",
                r#"{"type":"session_info","id":"a","name":"Fix the bug"}"#,
                "\n",
            ),
        )
        .unwrap();

        // `PI_HOME` is the only env reader in this module; no other test uses it.
        std::env::set_var("PI_HOME", &root);
        let title = agent().session_title("ses-1234", cwd);
        std::env::remove_var("PI_HOME");

        assert_eq!(title.as_deref(), Some("Fix the bug"));
        assert!(agent().has_session_titles());
        let _ = std::fs::remove_dir_all(&root);
    }
}
