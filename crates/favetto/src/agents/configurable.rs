//! Template-driven agent: the `ConfigurableAgent` fallback and the shared
//! [`TemplateAgent`] renderer embedded by the built-ins.
//!
//! The renderer is a faithful port of the invocation logic that used to live in
//! `AgentManager::start`, so a config that only sets `command` + templates
//! behaves exactly as before.

use std::path::PathBuf;
use std::sync::Arc;

use favetto_core::model::AgentCapabilities;

use crate::config::AgentConfig;

use super::agent::{
    append_template, substitute, Agent, AgentContext, AgentDescriptor, CommandSpec, Invocation,
    SessionIdProbe, SubmitStrategy, SUBMIT_DELAY, SUBMIT_MAX_SENDS,
};

/// Render an [`AgentConfig`]'s invocation templates into a [`CommandSpec`].
#[derive(Debug, Clone)]
pub(crate) struct TemplateAgent {
    pub(crate) descriptor: AgentDescriptor,
    pub(crate) config: AgentConfig,
    pub(crate) probe: Option<SessionIdProbe>,
}

impl TemplateAgent {
    /// Build the command for `invocation`, reading the owned per-launch values
    /// from `ctx`.
    pub(crate) fn command(
        &self,
        invocation: &Invocation<'_>,
        ctx: &AgentContext,
    ) -> anyhow::Result<CommandSpec> {
        let name = &self.descriptor.id;
        let cfg = &self.config;
        if cfg.command.trim().is_empty() {
            anyhow::bail!("agent '{name}' has no command configured");
        }

        let provider = ctx.provider.as_deref().unwrap_or("");
        let model = ctx.model.as_deref();
        let prompt = ctx.prompt.as_deref();

        let mut args: Vec<String> = Vec::new();
        let mut stdin_prompt: Option<String> = None;
        let mut stdin_eof = false;
        let mut submit = SubmitStrategy::None;

        match invocation {
            Invocation::Interactive { .. } => {
                // A selected model may require a different interactive entry point
                // (e.g. opencode's `mini --model`), since its main TUI has no flag.
                let base = if model.is_some() && cfg.interactive_model_args.is_some() {
                    cfg.interactive_model_args.as_deref().unwrap_or(&cfg.args)
                } else {
                    cfg.args.as_slice()
                };
                args.extend(substitute(
                    base,
                    &[("{provider}", provider), ("{model}", model.unwrap_or(""))],
                ));

                let mut prompt_via_args = false;
                if let (Some(p), Some(prompt_args)) = (prompt, cfg.prompt_args.as_ref()) {
                    let vars = [
                        ("{prompt}", p),
                        ("{provider}", provider),
                        ("{model}", model.unwrap_or("")),
                    ];
                    prompt_via_args = append_template(&mut args, prompt_args, &vars);
                }
                if !prompt_via_args {
                    stdin_prompt = prompt.map(str::to_string);
                }
                // A pre-filled prompt only needs Enter when the CLI does not submit
                // it itself (e.g. opencode's `--prompt`).
                if prompt_via_args && cfg.submit_prompt.unwrap_or(false) {
                    submit = SubmitStrategy::AfterSettle {
                        delay: SUBMIT_DELAY,
                        max_sends: SUBMIT_MAX_SENDS,
                    };
                }
            }
            Invocation::Headless { .. } => {
                // A headless run reading stdin needs EOF after the prompt.
                stdin_eof = true;
                let template = if model.is_some() {
                    cfg.run_args.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "agent '{name}' has no `run_args`, so it cannot run a task with a \
                             model; add one, e.g. run_args = [\"run\", \"--model\", \
                             \"{{provider}}/{{model}}\", \"--format\", \"json\", \"{{prompt}}\"]"
                        )
                    })?
                } else {
                    cfg.headless_args.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "agent '{name}' has no `headless_args`, so it cannot run a task \
                             unattended; add one, e.g. headless_args = [\"run\", \"--auto\", \
                             \"{{prompt}}\"]"
                        )
                    })?
                };
                let vars = [
                    ("{prompt}", prompt.unwrap_or("")),
                    ("{provider}", provider),
                    ("{model}", model.unwrap_or("")),
                    ("{session_id}", ctx.session_id.as_deref().unwrap_or("")),
                ];
                let prompt_via_args = append_template(&mut args, template, &vars);
                if !prompt_via_args {
                    stdin_prompt = prompt.map(str::to_string);
                }
            }
            Invocation::Resume(_) => {
                let template = cfg.resume_args.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "agent '{name}' has no `resume_args`, so it cannot reattach to a session"
                    )
                })?;
                append_template(
                    &mut args,
                    template,
                    &[("{session_id}", ctx.session_id.as_deref().unwrap_or(""))],
                );
            }
        }

        Ok(CommandSpec {
            program: PathBuf::from(&cfg.command),
            args,
            env: cfg.env.clone(),
            cwd: ctx.cwd.clone().or_else(|| cfg.cwd.clone()),
            stdin_prompt,
            stdin_eof,
            submit,
        })
    }
}

/// Overlay the fields present in `overrides` onto `base`.
///
/// "Present" means a non-empty `command`/`args`, a `Some(..)` option, a
/// `Some(..)` `submit_prompt`, a non-empty `env`, or a `Some(..)` `cwd`. This is
/// how a `[agents.<name>]` entry customizes a built-in's defaults: anything left
/// out keeps the built-in value.
pub(crate) fn overlay(base: &mut AgentConfig, overrides: &AgentConfig) {
    if !overrides.command.trim().is_empty() {
        base.command = overrides.command.clone();
    }
    if !overrides.args.is_empty() {
        base.args = overrides.args.clone();
    }
    if let Some(v) = &overrides.headless_args {
        base.headless_args = Some(v.clone());
    }
    if let Some(v) = &overrides.run_args {
        base.run_args = Some(v.clone());
    }
    if let Some(v) = &overrides.resume_args {
        base.resume_args = Some(v.clone());
    }
    if let Some(v) = &overrides.interactive_model_args {
        base.interactive_model_args = Some(v.clone());
    }
    if let Some(v) = &overrides.session_id_json_key {
        base.session_id_json_key = Some(v.clone());
    }
    if let Some(v) = &overrides.prompt_args {
        base.prompt_args = Some(v.clone());
    }
    if let Some(v) = overrides.submit_prompt {
        base.submit_prompt = Some(v);
    }
    if !overrides.env.is_empty() {
        for (k, v) in &overrides.env {
            base.env.insert(k.clone(), v.clone());
        }
    }
    if let Some(v) = &overrides.cwd {
        base.cwd = Some(v.clone());
    }
}

/// Derive the wire capabilities of a template-only agent.
pub(crate) fn capabilities_from_config(config: &AgentConfig) -> AgentCapabilities {
    AgentCapabilities {
        interactive: true,
        headless: config.headless_args.is_some() || config.run_args.is_some(),
        resume: config.resume_args.is_some(),
        model_selection: config.run_args.is_some() || config.interactive_model_args.is_some(),
        providers: false,
        structured_output: config.session_id_json_key.is_some(),
        reports_session_id: config.session_id_json_key.is_some(),
        prompt_prefill: config.submit_prompt.unwrap_or(false),
    }
}

/// A fully template-driven agent, used when a config entry names no built-in.
pub struct ConfigurableAgent {
    template: TemplateAgent,
}

impl ConfigurableAgent {
    /// Build the agent for the `[agents.<name>]` entry `config`.
    pub fn from_config(name: &str, config: &AgentConfig) -> Self {
        let capabilities = capabilities_from_config(config);
        let probe = config
            .session_id_json_key
            .clone()
            .map(SessionIdProbe::JsonKey);
        let descriptor = AgentDescriptor {
            id: name.to_string(),
            name: name.to_string(),
            command: config.command.clone(),
            available: true,
            capabilities,
        };
        Self {
            template: TemplateAgent {
                descriptor,
                config: config.clone(),
                probe,
            },
        }
    }
}

impl Agent for ConfigurableAgent {
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

    fn set_available(&mut self, available: bool) {
        self.template.descriptor.available = available;
    }

    fn parse_output(&self, raw: &str, exit_code: Option<i32>) -> super::agent::AgentRunResult {
        let session_id = self
            .template
            .probe
            .as_ref()
            .and_then(|probe| super::agent::extract_session_id_from_lines(raw, probe));
        super::agent::AgentRunResult {
            exit_code,
            session_id,
            output: serde_json::json!({ "text": raw }),
            raw: raw.to_string(),
        }
    }

    fn provider_source(&self) -> Option<Arc<dyn super::agent::ProviderSource>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AgentConfig {
        AgentConfig {
            command: "mycli".to_string(),
            args: vec!["--tui".to_string()],
            headless_args: Some(vec!["run".to_string(), "{prompt}".to_string()]),
            session_id_json_key: Some("sessionID".to_string()),
            submit_prompt: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn configurable_capabilities_follow_templates() {
        let agent = ConfigurableAgent::from_config("mycli", &config());
        let caps = agent.capabilities();
        assert!(caps.interactive);
        assert!(caps.headless);
        assert!(!caps.resume);
        assert!(!caps.model_selection);
        assert!(!caps.providers);
        assert!(caps.structured_output);
        assert!(caps.reports_session_id);
        assert!(caps.prompt_prefill);
    }

    #[test]
    fn headless_substitutes_session_id() {
        let mut config = config();
        config.headless_args = Some(vec![
            "run".to_string(),
            "--session-id".to_string(),
            "{session_id}".to_string(),
            "{prompt}".to_string(),
        ]);
        let agent = ConfigurableAgent::from_config("mycli", &config);
        let ctx = AgentContext {
            prompt: Some("hi".to_string()),
            session_id: Some("ses-9".to_string()),
            ..Default::default()
        };
        let spec = agent
            .command(
                &Invocation::Headless {
                    prompt: "hi",
                    provider: None,
                    model: None,
                },
                &ctx,
            )
            .unwrap();
        assert_eq!(spec.args, vec!["run", "--session-id", "ses-9", "hi"]);
    }

    #[test]
    fn template_only_config_stays_non_provider() {
        // A bare command+args entry must not advertise a provider catalog.
        let bare = AgentConfig {
            command: "mycli".to_string(),
            ..Default::default()
        };
        let agent = ConfigurableAgent::from_config("mycli", &bare);
        let caps = agent.capabilities();
        assert!(caps.interactive);
        assert!(!caps.headless);
        assert!(!caps.providers);
        assert!(!caps.model_selection);
        assert!(agent.provider_source().is_none());
    }
}
