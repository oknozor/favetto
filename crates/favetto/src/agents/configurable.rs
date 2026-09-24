//! Template-driven agent: the `ConfigurableAgent` fallback and the shared
//! [`TemplateAgent`] renderer embedded by the built-ins.
//!
//! The renderer is a faithful port of the invocation logic that used to live in
//! `AgentManager::start`, so a config that only sets `command` + templates
//! behaves exactly as before.

use std::path::PathBuf;

use favetto_core::model::AgentCapabilities;

use crate::config::AgentConfig;

use super::agent::{
    append_template, substitute, Agent, AgentContext, AgentDescriptor, CommandSpec, Invocation,
    SessionIdProbe, SubmitStrategy, SUBMIT_DELAY, SUBMIT_MAX_SENDS,
};

/// Delegate the four shared `Agent` methods to an adapter's inner
/// [`TemplateAgent`]: `descriptor`, `command`, `session_id_probe` and
/// `set_available`.
///
/// Call it as the first item in a built-in's `impl Agent` block; every genuine
/// per-agent difference (`parse_output`, `session_title`, `provider_source`,
/// `awaiting_input`, …) stays as a hand-written override in the same block, so
/// the block reads as a list of what actually differs.
///
/// The types are spelled through `$crate` so a call site only needs `Agent` in
/// scope, not the underlying `Invocation`/`CommandSpec`/… types.
macro_rules! delegate_to_template {
    () => {
        fn descriptor(&self) -> &$crate::agents::agent::AgentDescriptor {
            &self.template.descriptor
        }

        fn command(
            &self,
            invocation: &$crate::agents::agent::Invocation<'_>,
            ctx: &$crate::agents::agent::AgentContext,
        ) -> anyhow::Result<$crate::agents::agent::CommandSpec> {
            self.template.command(invocation, ctx)
        }

        fn session_id_probe(&self) -> Option<$crate::agents::agent::SessionIdProbe> {
            self.template.probe.clone()
        }

        fn set_available(&mut self, available: bool) {
            self.template.descriptor.available = available;
        }
    };
}

pub(crate) use delegate_to_template;

/// Render an [`AgentConfig`]'s invocation templates into a [`CommandSpec`].
#[derive(Debug, Clone)]
pub(crate) struct TemplateAgent {
    pub(crate) descriptor: AgentDescriptor,
    pub(crate) config: AgentConfig,
    pub(crate) probe: Option<SessionIdProbe>,
}

impl TemplateAgent {
    /// Build the shared renderer for a built-in (or template-only) agent.
    ///
    /// `id` is the configured `[agents.<name>]` key and `name` the human-readable
    /// CLI name. The wire capabilities and the session-id probe are both derived
    /// from `config`, so a caller only supplies what actually differs per agent.
    pub(crate) fn new(id: &str, name: &str, config: AgentConfig) -> Self {
        let capabilities = capabilities_from_config(&config);
        let probe = config
            .session_id_json_key
            .clone()
            .map(SessionIdProbe::JsonKey);
        let descriptor = AgentDescriptor {
            id: id.to_string(),
            name: name.to_string(),
            command: config.command.clone(),
            available: true,
            capabilities,
        };
        Self {
            descriptor,
            config,
            probe,
        }
    }

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
        interactive_prompt: config.prompt_args.is_some() || config.submit_prompt.unwrap_or(false),
        reports_state: false,
        permission_channel: false,
    }
}

/// A fully template-driven agent, used when a config entry names no built-in.
pub struct ConfigurableAgent {
    template: TemplateAgent,
}

impl ConfigurableAgent {
    /// Build the agent for the `[agents.<name>]` entry `config`.
    pub fn from_config(name: &str, config: &AgentConfig) -> Self {
        Self {
            template: TemplateAgent::new(name, name, config.clone()),
        }
    }
}

impl Agent for ConfigurableAgent {
    delegate_to_template!();

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
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::GitSettings;

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
        assert!(caps.interactive_prompt);
    }

    #[test]
    fn interactive_prompt_follows_prompt_templates() {
        // `prompt_args` alone is enough to seed an interactive TUI.
        let with_prompt_args = AgentConfig {
            command: "mycli".to_string(),
            prompt_args: Some(vec!["--prompt".to_string(), "{prompt}".to_string()]),
            ..Default::default()
        };
        assert!(
            ConfigurableAgent::from_config("mycli", &with_prompt_args)
                .capabilities()
                .interactive_prompt
        );

        // `submit_prompt` alone also counts (the prompt goes to stdin).
        let submit_only = AgentConfig {
            command: "mycli".to_string(),
            submit_prompt: Some(true),
            ..Default::default()
        };
        assert!(
            ConfigurableAgent::from_config("mycli", &submit_only)
                .capabilities()
                .interactive_prompt
        );

        // A bare command has no way to seed the prompt.
        let bare = AgentConfig {
            command: "mycli".to_string(),
            ..Default::default()
        };
        assert!(
            !ConfigurableAgent::from_config("mycli", &bare)
                .capabilities()
                .interactive_prompt
        );
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
    fn template_agent_new_derives_capabilities_and_probe() {
        // `TemplateAgent::new` is the single construction path shared by every
        // adapter; it must derive both capabilities and the session-id probe.
        let template = TemplateAgent::new("mycli", "MyCLI", config());
        assert_eq!(template.descriptor.id, "mycli");
        assert_eq!(template.descriptor.name, "MyCLI");
        assert_eq!(template.descriptor.command, "mycli");
        assert!(template.descriptor.available);
        assert_eq!(
            template.probe,
            Some(SessionIdProbe::JsonKey("sessionID".to_string()))
        );
        assert!(template.descriptor.capabilities.headless);
        assert!(template.descriptor.capabilities.reports_session_id);
        assert!(template.descriptor.capabilities.prompt_prefill);
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

    #[test]
    fn overlay_copies_every_template_field() {
        // Exhaustive literal on purpose: no `..Default::default()`, so adding a
        // field to `AgentConfig` breaks compilation here until `overlay` and the
        // copy assertions below are updated. `agent_type` and `git` are listed
        // too even though they are deliberately *not* overlaid (dispatch and git
        // provisioning are handled elsewhere).
        let overrides = AgentConfig {
            agent_type: Some("override-type".to_string()),
            command: "override".to_string(),
            args: vec!["--override".to_string()],
            headless_args: Some(vec!["override-headless".to_string()]),
            run_args: Some(vec!["override-run".to_string()]),
            resume_args: Some(vec!["override-resume".to_string()]),
            interactive_model_args: Some(vec!["override-model".to_string()]),
            session_id_json_key: Some("overrideSessionId".to_string()),
            prompt_args: Some(vec!["override-prompt".to_string()]),
            submit_prompt: Some(true),
            env: BTreeMap::from([
                ("SHARED".to_string(), "override".to_string()),
                ("OVERRIDE_ONLY".to_string(), "1".to_string()),
            ]),
            cwd: Some(PathBuf::from("/override")),
            git: Some(GitSettings {
                user_name: Some("override".to_string()),
                ..Default::default()
            }),
        };

        let base = AgentConfig {
            agent_type: Some("base-type".to_string()),
            command: "base".to_string(),
            args: vec!["--base".to_string()],
            headless_args: Some(vec!["base-headless".to_string()]),
            run_args: Some(vec!["base-run".to_string()]),
            resume_args: Some(vec!["base-resume".to_string()]),
            interactive_model_args: Some(vec!["base-model".to_string()]),
            session_id_json_key: Some("baseSessionId".to_string()),
            prompt_args: Some(vec!["base-prompt".to_string()]),
            submit_prompt: Some(false),
            env: BTreeMap::from([
                ("SHARED".to_string(), "base".to_string()),
                ("BASE_ONLY".to_string(), "1".to_string()),
            ]),
            cwd: Some(PathBuf::from("/base")),
            git: Some(GitSettings {
                user_name: Some("base".to_string()),
                ..Default::default()
            }),
        };
        let base_before = base.clone();

        let mut merged = base;
        overlay(&mut merged, &overrides);

        // Every template field is copied from the override.
        assert_eq!(merged.command, "override");
        assert_eq!(merged.args, vec!["--override"]);
        assert_eq!(merged.headless_args, overrides.headless_args);
        assert_eq!(merged.run_args, overrides.run_args);
        assert_eq!(merged.resume_args, overrides.resume_args);
        assert_eq!(
            merged.interactive_model_args,
            overrides.interactive_model_args
        );
        assert_eq!(merged.session_id_json_key, overrides.session_id_json_key);
        assert_eq!(merged.prompt_args, overrides.prompt_args);
        assert_eq!(merged.submit_prompt, Some(true));
        assert_eq!(merged.cwd, overrides.cwd);

        // `env` merges key-by-key: the override wins for shared keys, new keys
        // are added, and base-only keys survive.
        assert_eq!(
            merged.env.get("SHARED").map(String::as_str),
            Some("override")
        );
        assert_eq!(merged.env.get("BASE_ONLY").map(String::as_str), Some("1"));
        assert_eq!(
            merged.env.get("OVERRIDE_ONLY").map(String::as_str),
            Some("1")
        );
        assert_eq!(merged.env.len(), 3);

        // Intentional non-overlays keep their base values.
        assert_eq!(merged.agent_type.as_deref(), Some("base-type"));
        assert_eq!(merged.git, base_before.git);
    }

    #[test]
    fn overlay_absent_template_fields_keep_base() {
        let base = AgentConfig {
            command: "base".to_string(),
            args: vec!["--base".to_string()],
            headless_args: Some(vec!["base-headless".to_string()]),
            run_args: Some(vec!["base-run".to_string()]),
            resume_args: Some(vec!["base-resume".to_string()]),
            interactive_model_args: Some(vec!["base-model".to_string()]),
            session_id_json_key: Some("baseSessionId".to_string()),
            prompt_args: Some(vec!["base-prompt".to_string()]),
            submit_prompt: Some(false),
            env: BTreeMap::from([("BASE_ONLY".to_string(), "1".to_string())]),
            cwd: Some(PathBuf::from("/base")),
            ..Default::default()
        };

        let mut merged = base.clone();
        // An all-default override must not clobber any base value.
        overlay(&mut merged, &AgentConfig::default());

        assert_eq!(merged.command, base.command);
        assert_eq!(merged.args, base.args);
        assert_eq!(merged.headless_args, base.headless_args);
        assert_eq!(merged.run_args, base.run_args);
        assert_eq!(merged.resume_args, base.resume_args);
        assert_eq!(merged.interactive_model_args, base.interactive_model_args);
        assert_eq!(merged.session_id_json_key, base.session_id_json_key);
        assert_eq!(merged.prompt_args, base.prompt_args);
        assert_eq!(merged.submit_prompt, base.submit_prompt);
        assert_eq!(merged.env, base.env);
        assert_eq!(merged.cwd, base.cwd);
    }
}
