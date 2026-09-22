//! Built-in agent for [opencode](https://opencode.ai).

use std::path::Path;
use std::sync::Arc;

use favetto_core::model::AwaitingInputReason;
use futures_util::future::BoxFuture;

use crate::config::AgentConfig;

use super::agent::{
    extract_session_id, Agent, AgentContext, AgentDescriptor, AgentRunResult, CommandSpec,
    Invocation, ProviderSource, SessionIdProbe,
};
use super::configurable::{capabilities_from_config, overlay, TemplateAgent};

/// The built-in defaults, matching `config.example.toml`.
fn base_config() -> AgentConfig {
    AgentConfig {
        command: "opencode".to_string(),
        args: Vec::new(),
        prompt_args: Some(vec!["--prompt".to_string(), "{prompt}".to_string()]),
        submit_prompt: Some(true),
        headless_args: Some(vec![
            "run".to_string(),
            "--auto".to_string(),
            "{prompt}".to_string(),
        ]),
        run_args: Some(vec![
            "run".to_string(),
            "--model".to_string(),
            "{provider}/{model}".to_string(),
            "--auto".to_string(),
            "--format".to_string(),
            "json".to_string(),
            "{prompt}".to_string(),
        ]),
        resume_args: Some(vec!["--session".to_string(), "{session_id}".to_string()]),
        interactive_model_args: Some(vec![
            "mini".to_string(),
            "--model".to_string(),
            "{provider}/{model}".to_string(),
        ]),
        session_id_json_key: Some("sessionID".to_string()),
        ..Default::default()
    }
}

/// The opencode implementation.
pub struct OpenCodeAgent {
    template: TemplateAgent,
}

impl OpenCodeAgent {
    /// Build opencode for the `[agents.<name>]` entry `overrides`.
    pub fn from_config(name: &str, overrides: &AgentConfig) -> Self {
        let mut config = base_config();
        overlay(&mut config, overrides);
        let mut capabilities = capabilities_from_config(&config);
        capabilities.providers = true;
        let probe = config
            .session_id_json_key
            .clone()
            .map(SessionIdProbe::JsonKey);
        let descriptor = AgentDescriptor {
            id: name.to_string(),
            name: "OpenCode".to_string(),
            command: config.command.clone(),
            available: true,
            capabilities,
        };
        Self {
            template: TemplateAgent {
                descriptor,
                config,
                probe,
            },
        }
    }
}

impl Agent for OpenCodeAgent {
    fn descriptor(&self) -> &AgentDescriptor {
        &self.template.descriptor
    }

    fn command(
        &self,
        invocation: &Invocation<'_>,
        ctx: &AgentContext,
    ) -> anyhow::Result<CommandSpec> {
        self.template.command(invocation, ctx)
    }

    fn session_id_probe(&self) -> Option<SessionIdProbe> {
        self.template.probe.clone()
    }

    fn has_session_titles(&self) -> bool {
        true
    }

    fn session_title(&self, session_id: &str, cwd: &Path) -> Option<String> {
        let out = std::process::Command::new(&self.template.config.command)
            .args(["session", "list", "--format", "json"])
            .current_dir(cwd)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        session_title_from_json(&String::from_utf8_lossy(&out.stdout), session_id)
    }

    fn set_available(&mut self, available: bool) {
        self.template.descriptor.available = available;
    }

    fn parse_output(&self, raw: &str, exit_code: Option<i32>) -> AgentRunResult {
        let mut events = Vec::new();
        let mut session_id = None;
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                // Not the structured format we expected: keep the raw text.
                return AgentRunResult {
                    exit_code,
                    session_id,
                    output: serde_json::json!({ "text": raw }),
                    raw: raw.to_string(),
                };
            };
            if session_id.is_none() {
                if let Some(probe) = &self.template.probe {
                    session_id = extract_session_id(&value, probe);
                }
            }
            events.push(value);
        }
        AgentRunResult {
            exit_code,
            session_id,
            output: serde_json::Value::Array(events),
            raw: raw.to_string(),
        }
    }

    fn provider_source(&self) -> Option<Arc<dyn ProviderSource>> {
        Some(Arc::new(OpenCodeProviderSource))
    }

    fn awaiting_input(&self, screen: &vt100::Screen) -> Option<AwaitingInputReason> {
        super::detect::opencode_awaiting_input(&screen.contents())
    }
}

/// Find `session_id`'s title in `opencode session list --format json` output:
/// a JSON array of `{ "id", "title", … }`.
fn session_title_from_json(raw: &str, session_id: &str) -> Option<String> {
    let sessions: Vec<serde_json::Value> = serde_json::from_str(raw).ok()?;
    sessions
        .iter()
        .find(|s| s.get("id").and_then(|v| v.as_str()) == Some(session_id))
        .and_then(|s| s.get("title").and_then(|v| v.as_str()))
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty())
}

/// Providers authenticated with opencode (`auth.json` + models.dev).
pub struct OpenCodeProviderSource;

impl ProviderSource for OpenCodeProviderSource {
    fn configured_providers(&self) -> anyhow::Result<Vec<String>> {
        favetto_providers::configured_providers()
    }

    fn fetch<'a>(
        &'a self,
        client: &'a reqwest::Client,
    ) -> BoxFuture<'a, anyhow::Result<Vec<favetto_providers::Provider>>> {
        Box::pin(async move {
            let configured = self.configured_providers()?;
            let url = std::env::var("OPENCODE_MODELS_URL")
                .unwrap_or_else(|_| favetto_providers::DEFAULT_CATALOG_URL.to_string());
            favetto_providers::fetch_catalog(client, &configured, &url).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::agent::SubmitStrategy;

    fn ctx(prompt: Option<&str>, provider: Option<&str>, model: Option<&str>) -> AgentContext {
        AgentContext {
            prompt: prompt.map(str::to_string),
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
            ..Default::default()
        }
    }

    fn agent() -> OpenCodeAgent {
        OpenCodeAgent::from_config("opencode", &AgentConfig::default())
    }

    #[test]
    fn interactive_without_prompt_uses_base_args() {
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
        assert_eq!(spec.program, std::path::PathBuf::from("opencode"));
        assert!(spec.args.is_empty());
        assert!(spec.stdin_prompt.is_none());
        assert_eq!(spec.submit, SubmitStrategy::None);
    }

    #[test]
    fn interactive_prompt_is_prefilled_and_submitted() {
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
        assert_eq!(spec.args, vec!["--prompt", "hi"]);
        assert!(spec.stdin_prompt.is_none());
        assert!(matches!(spec.submit, SubmitStrategy::AfterSettle { .. }));
    }

    #[test]
    fn interactive_model_uses_mini_entrypoint() {
        let spec = agent()
            .command(
                &Invocation::Interactive {
                    prompt: None,
                    provider: Some("jev"),
                    model: Some("1.13"),
                },
                &ctx(None, Some("jev"), Some("1.13")),
            )
            .unwrap();
        assert_eq!(spec.args, vec!["mini", "--model", "jev/1.13"]);
    }

    #[test]
    fn headless_without_model_uses_headless_args() {
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
        assert_eq!(spec.args, vec!["run", "--auto", "do it"]);
        assert!(spec.stdin_eof);
        assert!(spec.stdin_prompt.is_none());
    }

    #[test]
    fn headless_with_model_uses_run_args() {
        let spec = agent()
            .command(
                &Invocation::Headless {
                    prompt: "do it",
                    provider: Some("jev"),
                    model: Some("1.13"),
                },
                &ctx(Some("do it"), Some("jev"), Some("1.13")),
            )
            .unwrap();
        assert_eq!(
            spec.args,
            vec!["run", "--model", "jev/1.13", "--auto", "--format", "json", "do it"]
        );
    }

    #[test]
    fn resume_uses_resume_args() {
        let ctx = AgentContext {
            session_id: Some("ses_1".to_string()),
            ..Default::default()
        };
        let spec = agent().command(&Invocation::Resume("ses_1"), &ctx).unwrap();
        assert_eq!(spec.args, vec!["--session", "ses_1"]);
    }

    #[test]
    fn all_capabilities_are_enabled() {
        let caps = agent().capabilities();
        assert!(caps.interactive);
        assert!(caps.headless);
        assert!(caps.resume);
        assert!(caps.model_selection);
        assert!(caps.providers);
        assert!(caps.structured_output);
        assert!(caps.reports_session_id);
        assert!(caps.prompt_prefill);
        assert!(agent().has_session_titles());
    }

    #[test]
    fn parse_output_reads_line_delimited_json() {
        let raw = "{\"sessionID\":\"ses_9\",\"type\":\"text\"}\n{\"type\":\"done\"}\n";
        let result = agent().parse_output(raw, Some(0));
        assert_eq!(result.session_id.as_deref(), Some("ses_9"));
        assert_eq!(result.output.as_array().unwrap().len(), 2);
        assert_eq!(result.output[0]["sessionID"], "ses_9");
    }

    #[test]
    fn parse_output_falls_back_to_text() {
        let result = agent().parse_output("plain output", Some(1));
        assert_eq!(result.output, serde_json::json!({ "text": "plain output" }));
        assert!(result.session_id.is_none());
    }

    #[test]
    fn session_title_from_json_matches_by_id() {
        let raw = r#"[
            {"id":"ses_1","title":"First session","updated":1},
            {"id":"ses_2","title":"Fix the widget","updated":2}
        ]"#;
        assert_eq!(
            session_title_from_json(raw, "ses_2").as_deref(),
            Some("Fix the widget")
        );
        // Unknown id and empty title both yield no title.
        assert!(session_title_from_json(raw, "ses_missing").is_none());
        assert!(session_title_from_json(r#"[{"id":"ses_1","title":"  "}]"#, "ses_1").is_none());
    }

    #[test]
    fn session_title_from_json_rejects_malformed() {
        assert!(session_title_from_json("not json", "ses_1").is_none());
        assert!(session_title_from_json(r#"{"id":"ses_1","title":"x"}"#, "ses_1").is_none());
    }

    #[test]
    fn session_title_from_json_absent_title_is_none() {
        // opencode v2.0.8 at t=0 lists the session before a title key exists.
        let raw = r#"[{"id":"ses_1","updated":1,"created":0,"projectId":"p","directory":"/tmp"}]"#;
        assert!(session_title_from_json(raw, "ses_1").is_none());
    }

    #[test]
    fn detects_permission_dialog_once_screen_rendered() {
        use favetto_core::model::AwaitingInputKind;
        let mut parser = vt100::Parser::new(10, 60, 0);
        parser.process(
            "Permission required\r\n❯ Allow once\r\n  Allow always\r\n  Reject".as_bytes(),
        );
        let reason = agent().awaiting_input(parser.screen());
        assert_eq!(reason.map(|r| r.kind), Some(AwaitingInputKind::Permission));
    }

    #[test]
    fn idle_screen_is_not_awaiting() {
        let mut parser = vt100::Parser::new(10, 60, 0);
        parser.process(b"Ask anything...");
        assert!(agent().awaiting_input(parser.screen()).is_none());
    }
}
