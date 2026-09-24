//! Built-in agent for [opencode](https://opencode.ai).

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use favetto_core::model::AwaitingInputReason;
use futures_util::future::BoxFuture;

use crate::config::AgentConfig;

use super::agent::{
    Agent, AgentContext, AgentDescriptor, AgentRunResult, CommandSpec, Invocation, ProviderSource,
    SessionIdProbe,
};
use super::configurable::{overlay, TemplateAgent};
use super::state::{StateSource, StateSourceConfig};

mod json;
mod observer;
mod server;
use json::OpenCodeJsonlParser;

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
        let mut template = TemplateAgent::new(name, "OpenCode", config);
        // opencode is the only built-in that exposes a provider/model catalog.
        template.descriptor.capabilities.providers = true;
        // The managed server observer reports live state and answers permissions.
        template.descriptor.capabilities.reports_state = true;
        template.descriptor.capabilities.permission_channel = true;
        // Headless runs go through the same managed server, so the panel can
        // attach a real interactive session to the run's session concurrently.
        template.descriptor.capabilities.concurrent_attach = true;
        Self { template }
    }
}

impl Agent for OpenCodeAgent {
    fn descriptor(&self) -> &AgentDescriptor {
        &self.template.descriptor
    }

    /// Render the template command, then route interactive/resume/headless
    /// launches through the managed server when one is running (and, for a fresh
    /// session, once `prepare_launch` has created and seeded it).
    fn command(
        &self,
        invocation: &Invocation<'_>,
        ctx: &AgentContext,
    ) -> anyhow::Result<CommandSpec> {
        let mut spec = self.template.command(invocation, ctx)?;
        let Some(endpoint) = server::endpoint() else {
            return Ok(spec);
        };
        match invocation {
            Invocation::Interactive { .. } => {
                // Only attach when the session was created by `prepare_launch`;
                // otherwise keep the `--prompt` + Enter fallback intact.
                if let Some(session_id) = ctx.session_id.as_deref() {
                    insert_server_args(&mut spec.args, &endpoint, Some(session_id));
                    spec.env.insert(
                        "OPENCODE_SERVER_PASSWORD".to_string(),
                        endpoint.password.clone(),
                    );
                }
            }
            Invocation::Resume(_) => {
                // `resume_args` already carries `--session {session_id}`.
                insert_server_args(&mut spec.args, &endpoint, None);
                spec.env.insert(
                    "OPENCODE_SERVER_PASSWORD".to_string(),
                    endpoint.password.clone(),
                );
            }
            Invocation::Headless { .. } => {
                // A headless run created its session on the managed server in
                // `prepare_launch`; point `run` at that server session so a
                // concurrent interactive attach can show the same turn. A
                // deterministic id that was never created on the server must not
                // be handed to `--session`.
                if ctx.managed_session {
                    if let Some(session_id) = ctx.session_id.as_deref() {
                        insert_server_args(&mut spec.args, &endpoint, Some(session_id));
                        spec.env.insert(
                            "OPENCODE_SERVER_PASSWORD".to_string(),
                            endpoint.password.clone(),
                        );
                    }
                }
            }
        }
        Ok(spec)
    }

    fn session_id_probe(&self) -> Option<SessionIdProbe> {
        self.template.probe.clone()
    }

    fn set_available(&mut self, available: bool) {
        self.template.descriptor.available = available;
    }

    fn has_session_titles(&self) -> bool {
        true
    }

    fn session_title(&self, session_id: &str, cwd: &Path) -> Option<String> {
        let mut cmd = std::process::Command::new(&self.template.config.command);
        cmd.args(["session", "list"]);
        if let Some(endpoint) = server::endpoint() {
            cmd.args(["--server", &endpoint.url]);
            cmd.env("OPENCODE_SERVER_PASSWORD", &endpoint.password);
        }
        let out = cmd
            .args(["--format", "json"])
            .current_dir(cwd)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        session_title_from_json(&String::from_utf8_lossy(&out.stdout), session_id)
    }

    fn parse_output(&self, raw: &str, exit_code: Option<i32>) -> AgentRunResult {
        let mut parser = OpenCodeJsonlParser::new(self.template.probe.clone());
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

    fn provider_source(&self) -> Option<Arc<dyn ProviderSource>> {
        Some(Arc::new(OpenCodeProviderSource))
    }

    fn awaiting_input(&self, screen: &vt100::Screen) -> Option<AwaitingInputReason> {
        super::detect::opencode_awaiting_input(&screen.contents())
    }

    /// The managed server is the live-state transport for interactive sessions.
    /// A headless run created on the managed server (`prepare_launch`) keeps
    /// its session too, so it is observed over the same SSE channel and reports
    /// live activity/usage without an Agent-panel visit. A headless run with no
    /// managed session keeps the tolerant stdout JSONL fallback.
    fn state_source(&self, cfg: &StateSourceConfig) -> Option<Box<dyn StateSource>> {
        if cfg.headless && !cfg.managed_session {
            return None;
        }
        let endpoint = server::endpoint()?;
        Some(Box::new(observer::OpenCodeServer::new(endpoint)))
    }

    /// Create the session on the managed server before the PTY spawns.
    ///
    /// Interactive launches also seed the prompt (the TUI then attaches to the
    /// seeded turn); headless launches only create the session, because the
    /// `run` process itself submits the prompt against `--session`. Any failure
    /// is swallowed: the launch then falls back to the old stdout path.
    fn prepare_launch<'a>(
        &'a self,
        invocation: &'a Invocation<'_>,
        ctx: &'a mut AgentContext,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let headless = match invocation {
                Invocation::Interactive { .. } => false,
                Invocation::Headless { .. } => true,
                Invocation::Resume(_) => return Ok(()),
            };
            // A headless launch's session id must come from the managed server:
            // drop the caller's deterministic placeholder so the fallback (no
            // server) does not hand `--session` a session that does not exist.
            if headless {
                ctx.session_id = None;
                ctx.managed_session = false;
            }
            let Some(endpoint) =
                server::ensure_started(Path::new(&self.template.config.command)).await
            else {
                return Ok(());
            };
            let Some(cwd) = ctx.cwd.clone() else {
                return Ok(());
            };
            let model = ctx.model.as_deref().zip(ctx.provider.as_deref());
            let session_id = match server::create_session(&endpoint, &cwd, model).await {
                Ok(session_id) => session_id,
                Err(e) => {
                    tracing::warn!(error = %e, "opencode session create failed; using the prompt fallback");
                    return Ok(());
                }
            };
            ctx.session_id = Some(session_id.clone());
            ctx.managed_session = true;
            if !headless {
                if let Some(prompt) = ctx.prompt.clone() {
                    match server::prompt(&endpoint, &session_id, &prompt).await {
                        Ok(()) => ctx.prompt = None,
                        Err(e) => {
                            tracing::warn!(error = %e, "opencode prompt seeding failed; using the prompt fallback");
                        }
                    }
                }
            }
            Ok(())
        })
    }

    /// Ask the managed server (when there is one) about the task's session,
    /// rather than waiting for the TUI to exit. The server tracks a per-session
    /// `outcome` ("succeeded"/"failed") once a turn completes.
    ///
    /// `opencode api GET /api/session` returns every session with its
    /// `location.directory` and `time.created`; we keep the newest one created in
    /// this task's working directory at or after `since`, so a leftover session
    /// from an earlier run in the same directory cannot be mistaken for this one.
    fn interactive_turn_done(&self, cwd: &Path, since: DateTime<Utc>) -> Option<bool> {
        let mut cmd = std::process::Command::new(&self.template.config.command);
        cmd.args(["api"]);
        if let Some(endpoint) = server::endpoint() {
            cmd.args(["--server", &endpoint.url]);
            cmd.env("OPENCODE_SERVER_PASSWORD", &endpoint.password);
        }
        let out = cmd
            .args(["GET", "/api/session"])
            .current_dir(cwd)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // opencode resolves the directory it was launched in; canonicalize ours
        // too so a symlinked path (e.g. `/tmp` on some hosts) compares equal.
        let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        interactive_turn_done_from_json(
            &String::from_utf8_lossy(&out.stdout),
            &cwd,
            since.timestamp_millis(),
        )
    }
}

/// Insert `--server <url>` (and, when known, `--session <id>`) after a leading
/// subcommand token (`mini`, `run`) or at the front.
fn insert_server_args(
    args: &mut Vec<String>,
    endpoint: &server::Endpoint,
    session_id: Option<&str>,
) {
    let mut introduced = vec!["--server".to_string(), endpoint.url.clone()];
    if let Some(session_id) = session_id {
        introduced.push("--session".to_string());
        introduced.push(session_id.to_string());
    }
    let at = if args
        .first()
        .map(|arg| arg == "mini" || arg == "run")
        .unwrap_or(false)
    {
        1
    } else {
        0
    };
    for (offset, arg) in introduced.into_iter().enumerate() {
        args.insert(at + offset, arg);
    }
}

/// Parse `opencode api GET /api/session` output and report the newest session in
/// `cwd` created at or after `since_ms`: `None` while it is still running (no
/// `outcome`), otherwise whether it succeeded.
fn interactive_turn_done_from_json(raw: &str, cwd: &Path, since_ms: i64) -> Option<bool> {
    let cwd = cwd.to_string_lossy();
    let response: serde_json::Value = serde_json::from_str(raw).ok()?;
    let sessions = response.get("data")?.as_array()?;
    let created_ms = |s: &serde_json::Value| {
        s.pointer("/time/created")
            .and_then(|t| t.as_i64())
            .unwrap_or(i64::MIN)
    };
    let session = sessions
        .iter()
        .filter(|s| s.pointer("/location/directory").and_then(|d| d.as_str()) == Some(cwd.as_ref()))
        .filter(|s| created_ms(s) >= since_ms)
        .max_by_key(|s| created_ms(s))?;
    let outcome = session.get("outcome").and_then(|o| o.as_str())?;
    Some(outcome == "succeeded")
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
    use crate::agents::agent::{AgentContext, Invocation, SubmitStrategy};
    use std::path::PathBuf;

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

    /// A headless run whose session was created on the managed server targets
    /// that server session, so the panel can attach to it concurrently.
    #[test]
    fn headless_with_managed_server_targets_the_session() {
        let endpoint = install_endpoint();
        let ctx = AgentContext {
            session_id: Some("ses_1".to_string()),
            managed_session: true,
            prompt: Some("do it".to_string()),
            ..Default::default()
        };
        let spec = agent()
            .command(
                &Invocation::Headless {
                    prompt: "do it",
                    provider: None,
                    model: None,
                },
                &ctx,
            )
            .unwrap();
        assert_eq!(
            spec.args,
            vec![
                "run".to_string(),
                "--server".to_string(),
                endpoint.url.clone(),
                "--session".to_string(),
                "ses_1".to_string(),
                "--auto".to_string(),
                "do it".to_string(),
            ]
        );
        assert_eq!(
            spec.env.get("OPENCODE_SERVER_PASSWORD").map(String::as_str),
            Some("pw")
        );
        server::install_endpoint_for_test(None);
    }

    /// Without a session created on the managed server, a headless run keeps
    /// the plain command even when a server endpoint exists.
    #[test]
    fn headless_without_a_managed_session_keeps_the_plain_run() {
        install_endpoint();
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
        assert!(!spec.env.contains_key("OPENCODE_SERVER_PASSWORD"));
        server::install_endpoint_for_test(None);
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
        assert!(caps.reports_state);
        assert!(caps.permission_channel);
        assert!(caps.concurrent_attach);
        assert!(agent().has_session_titles());
    }

    fn install_endpoint() -> server::Endpoint {
        let endpoint = server::Endpoint {
            url: "http://127.0.0.1:9".to_string(),
            password: "pw".to_string(),
        };
        server::install_endpoint_for_test(Some(endpoint.clone()));
        endpoint
    }

    #[test]
    fn interactive_with_managed_server_attaches_the_session() {
        let endpoint = install_endpoint();
        let ctx = AgentContext {
            session_id: Some("ses_1".to_string()),
            ..Default::default()
        };
        let spec = agent()
            .command(
                &Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                &ctx,
            )
            .unwrap();
        assert_eq!(
            spec.args,
            vec![
                "--server".to_string(),
                endpoint.url.clone(),
                "--session".to_string(),
                "ses_1".to_string(),
            ]
        );
        assert_eq!(
            spec.env.get("OPENCODE_SERVER_PASSWORD").map(String::as_str),
            Some("pw")
        );
        server::install_endpoint_for_test(None);
    }

    #[test]
    fn interactive_with_model_keeps_the_mini_subcommand() {
        let endpoint = install_endpoint();
        let ctx = AgentContext {
            session_id: Some("ses_1".to_string()),
            provider: Some("jev".to_string()),
            model: Some("1.13".to_string()),
            ..Default::default()
        };
        let spec = agent()
            .command(
                &Invocation::Interactive {
                    prompt: None,
                    provider: Some("jev"),
                    model: Some("1.13"),
                },
                &ctx,
            )
            .unwrap();
        assert_eq!(
            spec.args,
            vec![
                "mini".to_string(),
                "--server".to_string(),
                endpoint.url.clone(),
                "--session".to_string(),
                "ses_1".to_string(),
                "--model".to_string(),
                "jev/1.13".to_string(),
            ]
        );
        server::install_endpoint_for_test(None);
    }

    /// Without a session created by `prepare_launch`, an endpoint alone must not
    /// change the old `--prompt` + timed-Enter fallback.
    #[test]
    fn interactive_without_a_created_session_keeps_the_prompt_fallback() {
        install_endpoint();
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
        assert!(matches!(spec.submit, SubmitStrategy::AfterSettle { .. }));
        server::install_endpoint_for_test(None);
    }

    #[test]
    fn resume_with_managed_server_targets_the_server() {
        let endpoint = install_endpoint();
        let ctx = AgentContext {
            session_id: Some("ses_1".to_string()),
            ..Default::default()
        };
        let spec = agent().command(&Invocation::Resume("ses_1"), &ctx).unwrap();
        // `resume_args` already carries `--session`, so only `--server` is added.
        assert_eq!(
            spec.args,
            vec![
                "--server".to_string(),
                endpoint.url.clone(),
                "--session".to_string(),
                "ses_1".to_string(),
            ]
        );
        assert_eq!(
            spec.env.get("OPENCODE_SERVER_PASSWORD").map(String::as_str),
            Some("pw")
        );
        server::install_endpoint_for_test(None);
    }

    /// A plain headless run has no server transport to observe; an interactive
    /// launch and a headless run that owns a managed session both do. Every
    /// variant still requires a live endpoint.
    #[test]
    fn state_source_needs_an_endpoint_and_a_managed_transport() {
        let headless = StateSourceConfig {
            headless: true,
            ..Default::default()
        };
        let managed_headless = StateSourceConfig {
            headless: true,
            managed_session: true,
            ..Default::default()
        };
        let interactive = StateSourceConfig::default();

        // Without an endpoint none of the variants can observe anything.
        assert!(agent().state_source(&headless).is_none());
        assert!(agent().state_source(&managed_headless).is_none());
        assert!(agent().state_source(&interactive).is_none());

        install_endpoint();
        // A headless run with no managed session keeps the stdout fallback.
        assert!(agent().state_source(&headless).is_none());
        // Managed headless and interactive launches get the SSE observer.
        assert!(agent().state_source(&managed_headless).is_some());
        assert!(agent().state_source(&interactive).is_some());
        server::install_endpoint_for_test(None);
    }

    fn mock_state() -> Arc<std::sync::Mutex<Vec<serde_json::Value>>> {
        Arc::new(std::sync::Mutex::new(Vec::new()))
    }

    async fn record_create(
        axum::extract::State(state): axum::extract::State<
            Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        >,
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> axum::Json<serde_json::Value> {
        state.lock().unwrap().push(body);
        axum::Json(serde_json::json!({ "data": { "id": "ses_mock" } }))
    }

    async fn record_prompt(
        axum::extract::State(state): axum::extract::State<
            Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        >,
        axum::extract::Path(_id): axum::extract::Path<String>,
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> axum::http::StatusCode {
        state.lock().unwrap().push(body);
        axum::http::StatusCode::NO_CONTENT
    }

    #[tokio::test]
    async fn prepare_launch_creates_and_seeds_the_session() {
        use axum::routing::post;
        let recorded = mock_state();
        let app = axum::Router::new()
            .route("/api/session", post(record_create))
            .route("/api/session/{id}/prompt", post(record_prompt))
            .with_state(recorded.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        server::install_endpoint_for_test(Some(server::Endpoint {
            url: format!("http://{addr}"),
            password: "pw".to_string(),
        }));

        let mut launch_ctx = AgentContext {
            cwd: Some(PathBuf::from("/tmp")),
            prompt: Some("do it".to_string()),
            ..Default::default()
        };
        agent()
            .prepare_launch(
                &Invocation::Interactive {
                    prompt: Some("do it"),
                    provider: None,
                    model: None,
                },
                &mut launch_ctx,
            )
            .await
            .unwrap();
        assert_eq!(launch_ctx.session_id.as_deref(), Some("ses_mock"));
        assert!(
            launch_ctx.prompt.is_none(),
            "a seeded prompt must not also be passed on the command line"
        );

        let bodies = recorded.lock().unwrap().clone();
        assert!(bodies
            .iter()
            .any(|b| b.pointer("/location/directory").is_some()));
        assert!(bodies
            .iter()
            .any(|b| b.get("text").and_then(|v| v.as_str()) == Some("do it")));
        server::install_endpoint_for_test(None);
    }

    /// A headless launch creates its session on the managed server but leaves the
    /// prompt for `run` to submit, and records the session as managed so the
    /// manager can surface it before the run emits anything.
    #[tokio::test]
    async fn prepare_launch_creates_a_headless_session_without_seeding() {
        use axum::routing::post;
        let recorded = mock_state();
        let app = axum::Router::new()
            .route("/api/session", post(record_create))
            .route("/api/session/{id}/prompt", post(record_prompt))
            .with_state(recorded.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        server::install_endpoint_for_test(Some(server::Endpoint {
            url: format!("http://{addr}"),
            password: "pw".to_string(),
        }));

        let mut launch_ctx = AgentContext {
            cwd: Some(PathBuf::from("/tmp")),
            prompt: Some("do it".to_string()),
            session_id: Some("deterministic".to_string()),
            ..Default::default()
        };
        agent()
            .prepare_launch(
                &Invocation::Headless {
                    prompt: "do it",
                    provider: None,
                    model: None,
                },
                &mut launch_ctx,
            )
            .await
            .unwrap();
        assert_eq!(launch_ctx.session_id.as_deref(), Some("ses_mock"));
        assert_eq!(
            launch_ctx.prompt.as_deref(),
            Some("do it"),
            "a headless run submits its own prompt, so it must not be pre-seeded"
        );
        assert!(launch_ctx.managed_session);

        let bodies = recorded.lock().unwrap().clone();
        assert!(
            bodies
                .iter()
                .any(|b| b.pointer("/location/directory").is_some()),
            "the managed session was not created: {bodies:?}"
        );
        assert!(
            !bodies
                .iter()
                .any(|b| b.get("text").and_then(|v| v.as_str()) == Some("do it")),
            "a headless launch must not seed the prompt: {bodies:?}"
        );
        server::install_endpoint_for_test(None);
    }

    async fn record_create_with_model(
        axum::extract::State(state): axum::extract::State<
            Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        >,
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> axum::Json<serde_json::Value> {
        state.lock().unwrap().push(body);
        axum::Json(serde_json::json!({ "data": { "id": "ses_model" } }))
    }

    #[tokio::test]
    async fn prepare_launch_sends_the_selected_model() {
        use axum::routing::post;
        let recorded = mock_state();
        let app = axum::Router::new()
            .route("/api/session", post(record_create_with_model))
            .with_state(recorded.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        server::install_endpoint_for_test(Some(server::Endpoint {
            url: format!("http://{addr}"),
            password: "pw".to_string(),
        }));
        let mut launch_ctx = AgentContext {
            cwd: Some(PathBuf::from("/tmp")),
            provider: Some("opencode".to_string()),
            model: Some("space-bunny".to_string()),
            ..Default::default()
        };
        agent()
            .prepare_launch(
                &Invocation::Interactive {
                    prompt: None,
                    provider: Some("opencode"),
                    model: Some("space-bunny"),
                },
                &mut launch_ctx,
            )
            .await
            .unwrap();
        let bodies = recorded.lock().unwrap().clone();
        assert_eq!(
            bodies[0].pointer("/model/id").and_then(|v| v.as_str()),
            Some("space-bunny")
        );
        assert_eq!(
            bodies[0]
                .pointer("/model/providerID")
                .and_then(|v| v.as_str()),
            Some("opencode")
        );
        server::install_endpoint_for_test(None);
    }

    #[test]
    fn parse_output_returns_a_run_summary() {
        let raw = "{\"sessionID\":\"ses_9\",\"type\":\"text\",\"part\":{\"text\":\"hi\"}}\n\
                   {\"sessionID\":\"ses_9\",\"type\":\"step_finish\",\"part\":{\"reason\":\"stop\",\
                   \"tokens\":{\"input\":2}}}";
        let result = agent().parse_output(raw, Some(0));
        assert_eq!(result.session_id.as_deref(), Some("ses_9"));
        assert_eq!(result.output["session_id"], "ses_9");
        assert_eq!(result.output["text"], "hi");
        assert_eq!(result.output["outcome"], "succeeded");
        assert_eq!(result.output["usage"]["input_tokens"], 2);
        assert_eq!(result.raw, raw);
    }

    #[test]
    fn parse_output_ignores_noise_and_keeps_the_session_id() {
        let raw = "not json\n\
                   {\"sessionID\":\"ses_7\",\"type\":\"text\",\"part\":{\"text\":\"ok\"}}\n\
                   { broken\n\
                   {\"type\":\"step_finish\",\"sessionID\":\"ses_7\",\"part\":{\"reason\":\"stop\"}}\n";
        let result = agent().parse_output(raw, Some(0));
        assert_eq!(result.session_id.as_deref(), Some("ses_7"));
        assert_eq!(result.output["text"], "ok");
        assert_eq!(result.output["outcome"], "succeeded");
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

    /// A session still in flight has no `outcome` (it is not a required field):
    /// the probe must not report it as finished, or an interactive task would
    /// complete before doing any work.
    #[test]
    fn interactive_turn_done_ignores_a_session_without_outcome() {
        let raw = r#"{"data":[{"id":"ses_1",
            "location":{"directory":"/work/a"},
            "time":{"created":2000}}]}"#;
        assert_eq!(
            interactive_turn_done_from_json(raw, Path::new("/work/a"), 1000),
            None
        );
    }

    #[test]
    fn interactive_turn_done_reads_the_outcome() {
        let raw = r#"{"data":[
            {"id":"ses_old","outcome":"succeeded",
             "location":{"directory":"/work/a"},"time":{"created":100}},
            {"id":"ses_new","outcome":"succeeded",
             "location":{"directory":"/work/a"},"time":{"created":2000}}
        ]}"#;
        assert_eq!(
            interactive_turn_done_from_json(raw, Path::new("/work/a"), 1000),
            Some(true)
        );

        // Any other terminal outcome (`failed`, `interrupted`) is not a success.
        for outcome in ["failed", "interrupted"] {
            let other = raw.replace(
                "\"ses_new\",\"outcome\":\"succeeded\"",
                &format!("\"ses_new\",\"outcome\":\"{outcome}\""),
            );
            assert_eq!(
                interactive_turn_done_from_json(&other, Path::new("/work/a"), 1000),
                Some(false),
                "outcome: {outcome}"
            );
        }
    }

    /// A finished session from an earlier run in the same directory must not
    /// complete the current task: only sessions created at/after `since` count.
    #[test]
    fn interactive_turn_done_ignores_sessions_older_than_since() {
        let raw = r#"{"data":[{"id":"ses_old","outcome":"succeeded",
            "location":{"directory":"/work/a"},
            "time":{"created":100}}]}"#;
        assert_eq!(
            interactive_turn_done_from_json(raw, Path::new("/work/a"), 1000),
            None
        );
        // A different directory is ignored even when newer.
        let other = raw.replace("/work/a", "/work/b");
        assert_eq!(
            interactive_turn_done_from_json(&other, Path::new("/work/a"), 1000),
            None
        );
    }

    #[test]
    fn interactive_turn_done_rejects_malformed_output() {
        assert_eq!(
            interactive_turn_done_from_json("not json", Path::new("/work/a"), 0),
            None
        );
        assert_eq!(
            interactive_turn_done_from_json("{}", Path::new("/work/a"), 0),
            None
        );
    }
}
