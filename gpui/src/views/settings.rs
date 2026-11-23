//! Settings: connection, agents, appearance and about.
//!
//! The endpoint draft is normalized by [`crate::config`] and previewed before
//! it is saved; "Test connection" pings the draft without touching the live
//! transport. Appearance changes apply immediately (the whole shell reads
//! [`ClientState::theme`]) and are persisted beside the connection settings.

use gpui::{
    div, prelude::*, px, Context, Div, Entity, FocusHandle, Hsla, IntoElement, Render,
    ScrollHandle, Stateful, Window,
};

use crate::components;
use crate::settings::{latency_label, ConnectionTest, SettingsFocus};
use crate::state::{ClientState, ConnectionState, SnapshotStatus};
use crate::text_element::state_text_element;
use crate::text_input::TextTarget;
use crate::theme::Theme;

/// Where the About card points.
const DOCS_URL: &str = "https://github.com/oknozor/favetto/blob/main/docs/design/gpui-client.md";

/// The values one connection text field renders.
struct TextFieldSpec<'a> {
    id: &'static str,
    label: &'a str,
    placeholder: &'a str,
    focused: bool,
    focus: SettingsFocus,
}

/// The Settings surface.
pub struct SettingsView {
    state: Entity<ClientState>,
    scroll: ScrollHandle,
    reconnect_focus: FocusHandle,
    test_focus: FocusHandle,
    save_focus: FocusHandle,
    defaults_focus: FocusHandle,
    docs_focus: FocusHandle,
    error_copy_focus: FocusHandle,
    agents_retry_focus: FocusHandle,
}

impl SettingsView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self {
            state,
            scroll: ScrollHandle::new(),
            reconnect_focus: cx.focus_handle().tab_index(20).tab_stop(true),
            test_focus: cx.focus_handle().tab_index(21).tab_stop(true),
            save_focus: cx.focus_handle().tab_index(22).tab_stop(true),
            defaults_focus: cx.focus_handle().tab_index(23).tab_stop(true),
            docs_focus: cx.focus_handle().tab_index(24).tab_stop(true),
            error_copy_focus: cx.focus_handle().tab_index(25).tab_stop(true),
            agents_retry_focus: cx.focus_handle().tab_index(26).tab_stop(true),
        }
    }

    fn header(&self, theme: &Theme) -> Div {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(theme.spacing.s3))
            .px(px(theme.spacing.s4))
            .py(px(theme.spacing.s3))
            .border_b_1()
            .border_color(theme.colors.border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(components::section_label(theme, "SETTINGS"))
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child("Connection, agents and appearance"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s1))
                    .child(components::keycap(theme, "tab"))
                    .child(hint(theme, "next control"))
                    .child(components::keycap(theme, "⌘↵"))
                    .child(hint(theme, "save")),
            )
    }

    fn text_field(
        &self,
        theme: &Theme,
        field: TextFieldSpec<'_>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let TextFieldSpec {
            id,
            label,
            placeholder,
            focused,
            focus,
        } = field;
        let target = match focus {
            SettingsFocus::Endpoint => TextTarget::SettingsEndpoint,
            SettingsFocus::Token => TextTarget::SettingsToken,
            _ => TextTarget::SettingsEndpoint,
        };
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .h(px(40.0))
            .px(px(theme.spacing.s3))
            .rounded(px(theme.radii.r4))
            .border_2()
            .border_color(if focused {
                theme.colors.accent
            } else {
                theme.colors.border
            })
            .bg(theme.colors.bg)
            .cursor_text()
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.focus_settings(Some(focus));
                    cx.notify();
                });
            }))
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(110.0))
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.muted)
                    .child(label.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family("monospace")
                    .text_size(px(theme.font.sm))
                    .child(
                        state_text_element(&self.state, target, theme, cx).placeholder(placeholder),
                    ),
            )
    }

    fn error_copy_button(
        &self,
        theme: &Theme,
        message: &str,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let message = message.to_string();
        components::copy_button(theme, id, "Copy error", &self.error_copy_focus).on_click(
            cx.listener(move |this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.copy_text(message.clone(), "the error", cx);
                    cx.notify();
                });
            }),
        )
    }

    fn connection_card(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let (token_source, focus, normalized, test, save_error, connection) = {
            let state = self.state.read(cx);
            let settings = state.settings();
            (
                state.config().token_source.label(),
                settings.focus(),
                settings.normalized(),
                settings.test().clone(),
                settings.save_error().map(str::to_string),
                state.connection().clone(),
            )
        };

        let endpoint_focused = focus == Some(SettingsFocus::Endpoint);
        let token_focused = focus == Some(SettingsFocus::Token);

        let endpoint_field = self.text_field(
            theme,
            TextFieldSpec {
                id: "settings-endpoint",
                label: "Daemon address",
                placeholder: "127.0.0.1:7878",
                focused: endpoint_focused,
                focus: SettingsFocus::Endpoint,
            },
            cx,
        );
        let token_field = self.text_field(
            theme,
            TextFieldSpec {
                id: "settings-token",
                label: "Token",
                placeholder: "Using the token file",
                focused: token_focused,
                focus: SettingsFocus::Token,
            },
            cx,
        );

        let preview = match &normalized {
            Ok(endpoint) => div()
                .flex()
                .items_center()
                .gap(px(theme.spacing.s1))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.success)
                .child("✓")
                .child(format!("Saves as {endpoint}"))
                .into_any_element(),
            Err(error) => div()
                .flex()
                .items_start()
                .gap(px(theme.spacing.s1))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.danger)
                .child("⚠")
                .child(error.to_string())
                .into_any_element(),
        };

        let test_line = match &test {
            ConnectionTest::Idle => hint(
                theme,
                "Test connection runs system.ping and reports the round trip.",
            )
            .into_any_element(),
            ConnectionTest::Running { endpoint } => div()
                .flex()
                .items_center()
                .gap(px(theme.spacing.s1))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.warning)
                .child(components::animated_icon(
                    "◐",
                    true,
                    theme.colors.warning,
                    theme.font.xs,
                    "settings-test",
                ))
                .child(format!("Testing {endpoint}…"))
                .into_any_element(),
            ConnectionTest::Ok {
                endpoint,
                latency_ms,
                cwd,
            } => div()
                .flex()
                .items_center()
                .gap(px(theme.spacing.s1))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.success)
                .child(format!(
                    "✓ {endpoint} answered in {} · cwd {cwd}",
                    latency_label(*latency_ms)
                ))
                .into_any_element(),
            ConnectionTest::Failed { endpoint, message } => {
                let copy = self.error_copy_button(theme, message, "settings-test-error-copy", cx);
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.danger)
                            .child(format!("⚠ {endpoint} failed: {message}")),
                    )
                    .child(copy)
                    .into_any_element()
            }
        };

        let test = components::with_focus(
            div()
                .id("settings-test")
                .h(px(36.0))
                .px(px(theme.spacing.s3))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(if focus == Some(SettingsFocus::Test) {
                    theme.colors.accent
                } else {
                    theme.colors.border
                })
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.test_focus)
                .hover(|style| style.bg(theme.colors.bg))
                .child("Test connection")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.focus_settings(Some(SettingsFocus::Test));
                        state.test_connection(cx);
                        cx.notify();
                    });
                })),
            theme,
        );

        let save = components::with_focus(
            div()
                .id("settings-save")
                .h(px(40.0))
                .px(px(theme.spacing.s4))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .rounded(px(theme.radii.r8))
                .bg(Hsla::from(theme.colors.accent).opacity(0.20))
                .text_size(px(theme.font.sm))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.colors.accent)
                .cursor_pointer()
                .track_focus(&self.save_focus)
                .child("Save")
                .child(components::keycap(theme, "⌘↵"))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.focus_settings(Some(SettingsFocus::Save));
                        state.save_connection_settings(cx);
                        cx.notify();
                    });
                })),
            theme,
        );

        let defaults = components::with_focus(
            div()
                .id("settings-defaults")
                .h(px(36.0))
                .px(px(theme.spacing.s3))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(if focus == Some(SettingsFocus::Defaults) {
                    theme.colors.accent
                } else {
                    theme.colors.border
                })
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.defaults_focus)
                .hover(|style| style.bg(theme.colors.bg))
                .child("Use defaults")
                .child(hint(theme, "(token file)"))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.use_default_settings(cx);
                        cx.notify();
                    });
                })),
            theme,
        );

        let reconnect_state = self.state.clone();
        let reconnect = components::with_focus(
            div()
                .id("settings-reconnect")
                .h(px(36.0))
                .px(px(theme.spacing.s3))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .bg(Hsla::from(theme.colors.accent).opacity(0.14))
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.reconnect_focus)
                .child("Reconnect")
                .on_click(cx.listener(move |_this, _event, _window, cx| {
                    reconnect_state.update(cx, |state, cx| {
                        state.reconnect();
                        cx.notify();
                    });
                })),
            theme,
        );

        card(theme)
            .child(components::section_label(theme, "CONNECTION"))
            .child(endpoint_field)
            .child(preview)
            .child(token_field)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s1))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child("Token source:")
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family("monospace")
                            .child(token_source),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(theme.spacing.s2))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .child(
                                div()
                                    .text_size(px(theme.font.sm))
                                    .text_color(theme.colors.muted)
                                    .child("Status"),
                            )
                            .child(components::connection_pill(theme, &connection)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .child(reconnect),
                    ),
            )
            .when(
                matches!(
                    &connection,
                    ConnectionState::Unauthorized { .. } | ConnectionState::Disconnected { .. }
                ),
                |card| {
                    let message = match &connection {
                        ConnectionState::Unauthorized { token_path } => format!(
                            "The daemon rejected the token. Replace or edit the token at {token_path}, then Save or Reconnect."
                        ),
                        ConnectionState::Disconnected { message } => format!(
                            "The client cannot reach the daemon: {message}. Check the address above and Reconnect."
                        ),
                        _ => String::new(),
                    };
                    card.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .rounded(px(theme.radii.r4))
                            .border_1()
                            .border_color(Hsla::from(theme.colors.danger).opacity(0.45))
                            .bg(Hsla::from(theme.colors.danger).opacity(0.10))
                            .px(px(theme.spacing.s3))
                            .py(px(theme.spacing.s2))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .text_size(px(theme.font.xs))
                                    .text_color(theme.colors.text)
                                    .child(message),
                            )
                            .child(self.error_copy_button(
                                theme,
                                &connection.label(),
                                "settings-connection-error-copy",
                                cx,
                            )),
                    )
                },
            )
            .child(test_line)
            .when_some(save_error, |card, error| {
                let copy = self.error_copy_button(theme, &error, "settings-save-error-copy", cx);
                card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(theme.spacing.s2))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.danger)
                                .child(error),
                        )
                        .child(copy),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .pt(px(theme.spacing.s1))
                    .child(test)
                    .child(defaults)
                    .child(div().flex_1())
                    .child(save),
            )
    }

    fn agents_card(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let (agents, status, has_snapshot) = {
            let state = self.state.read(cx);
            (
                state.agent_roster().to_vec(),
                state.snapshot_status().clone(),
                state.has_snapshot_data(),
            )
        };
        let notice: Option<Div> = match &status {
            SnapshotStatus::Loading if agents.is_empty() => Some(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(components::section_label(theme, "AGENTS"))
                    .child(hint(theme, "Loading the daemon roster…")),
            ),
            SnapshotStatus::Failed { message } if agents.is_empty() => {
                let copy = self.error_copy_button(theme, message, "settings-agents-error-copy", cx);
                let retry = components::with_focus(
                    div()
                        .id("settings-agents-retry")
                        .h(px(theme.controls.min))
                        .px(px(theme.spacing.s3))
                        .flex()
                        .items_center()
                        .rounded(px(theme.radii.r8))
                        .bg(Hsla::from(theme.colors.accent).opacity(0.16))
                        .text_size(px(theme.font.sm))
                        .text_color(theme.colors.accent)
                        .cursor_pointer()
                        .track_focus(&self.agents_retry_focus)
                        .child("Retry")
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.state.update(cx, |state, cx| {
                                state.refresh_snapshot();
                                cx.notify();
                            });
                        })),
                    theme,
                );
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(theme.spacing.s2))
                        .child(
                            div()
                                .text_size(px(theme.font.sm))
                                .text_color(theme.colors.text)
                                .child("The agent roster could not load."),
                        )
                        .child(
                            div()
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.muted)
                                .child(message.to_string()),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(theme.spacing.s2))
                                .child(copy)
                                .child(retry),
                        ),
                )
            }
            SnapshotStatus::Failed { message } if has_snapshot => {
                let copy =
                    self.error_copy_button(theme, message, "settings-agents-banner-copy", cx);
                Some(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(theme.spacing.s2))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.text)
                                .child(format!("Roster refresh failed: {message}")),
                        )
                        .child(copy),
                )
            }
            _ => None,
        };
        let mut rows: Vec<gpui::AnyElement> = if agents.is_empty() {
            vec![div()
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.muted)
                .child(
                    "No agents are reported yet. The roster arrives with the shared agents.list snapshot; check the connection above if this stays empty.",
                )
                .into_any_element()]
        } else {
            agents
                .iter()
                .enumerate()
                .map(|(agent_index, agent)| {
                    let (dot, availability, color) = if agent.available {
                        ("●", "available", theme.colors.success)
                    } else {
                        ("○", "missing", theme.colors.danger)
                    };
                    let backend = if agent.capabilities.acp { "acp" } else { "pty" };
                    div()
                        .id(("settings-agent", agent_index))
                        .flex()
                        .items_center()
                        .gap(px(theme.spacing.s3))
                        .min_h(px(36.0))
                        .px(px(theme.spacing.s2))
                        .py(px(theme.spacing.s1))
                        .border_b_1()
                        .border_color(theme.colors.border)
                        .child(
                            div()
                                .w(px(170.0))
                                .flex_shrink_0()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(px(theme.font.sm))
                                        .text_color(theme.colors.text)
                                        .child(if agent.display_name.is_empty() {
                                            agent.name.clone()
                                        } else {
                                            agent.display_name.clone()
                                        }),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .font_family("monospace")
                                        .text_size(px(theme.font.xs))
                                        .text_color(theme.colors.muted)
                                        .child(agent.name.clone()),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family("monospace")
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.text)
                                .child(agent.command.clone()),
                        )
                        .child(
                            div()
                                .w(px(96.0))
                                .flex_shrink_0()
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.text)
                                .child(backend),
                        )
                        .child(
                            div()
                                .w(px(104.0))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .gap(px(theme.spacing.s1))
                                .text_size(px(theme.font.xs))
                                .text_color(color)
                                .child(dot)
                                .child(div().text_color(theme.colors.text).child(availability)),
                        )
                        .child(
                            div()
                                .w(px(76.0))
                                .flex_shrink_0()
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.muted)
                                .child(format!("{} live", agent.sessions.len())),
                        )
                        .into_any_element()
                })
                .collect()
        };

        // ACP authentication is session-scoped. Surface the safe next step for
        // every live session that needs sign-in (or can sign out), without
        // assuming this client can launch the daemon's agent command.
        let mut auth_row_id = 0usize;
        for agent in &agents {
            for session in &agent.sessions {
                let Some(auth) = &session.auth else {
                    continue;
                };
                let supported: Vec<_> = auth
                    .methods
                    .iter()
                    .filter(|method| method.supported)
                    .collect();
                if supported.is_empty() && !auth.logout_supported {
                    continue;
                }
                let terminal_only = supported.is_empty()
                    && auth.methods.iter().any(|method| {
                        method.kind == favetto_wire::model::AgentAuthMethodKind::Terminal
                    });
                let headline = match auth.status {
                    favetto_wire::model::AgentAuthStatus::Required => "needs sign-in",
                    favetto_wire::model::AgentAuthStatus::Authenticating => "signing in…",
                    favetto_wire::model::AgentAuthStatus::Failed => "sign-in failed",
                    favetto_wire::model::AgentAuthStatus::Authenticated => "signed in",
                    favetto_wire::model::AgentAuthStatus::NotRequired => "signed in",
                };
                let detail = auth
                    .message
                    .clone()
                    .or_else(|| {
                        terminal_only.then(|| {
                            "Terminal sign-in cannot run remotely; configure credentials on the daemon host."
                                .to_string()
                        })
                    })
                    .unwrap_or_else(|| {
                        format!(
                            "session {} · the daemon runs the agent's own sign-in flow",
                            &session.id[..session.id.len().min(8)]
                        )
                    });
                let mut actions = div().flex().items_center().gap(px(theme.spacing.s1));
                for method in &supported {
                    let session_id = session.id.clone();
                    let method_id = method.id.clone();
                    let enabled =
                        auth.status != favetto_wire::model::AgentAuthStatus::Authenticating;
                    let row_id = auth_row_id;
                    auth_row_id = auth_row_id.wrapping_add(1);
                    actions = actions.child(components::with_focus(
                        div()
                            .id(("settings-agent-auth", row_id))
                            .h(px(theme.controls.min))
                            .px(px(theme.spacing.s2))
                            .flex()
                            .items_center()
                            .rounded(px(theme.radii.r8))
                            .text_size(px(theme.font.xs))
                            .when(enabled, |button| {
                                button
                                    .bg(Hsla::from(theme.colors.accent).opacity(0.16))
                                    .text_color(theme.colors.accent)
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        this.state.update(cx, |state, cx| {
                                            state.authenticate(
                                                session_id.clone(),
                                                method_id.clone(),
                                                cx,
                                            );
                                            cx.notify();
                                        });
                                    }))
                            })
                            .when(!enabled, |button| {
                                button
                                    .text_color(theme.colors.muted)
                                    .opacity(0.55)
                                    .cursor_default()
                            })
                            .child(format!("Sign in with {}", method.name)),
                        theme,
                    ));
                }
                if auth.logout_supported {
                    let session_id = session.id.clone();
                    let row_id = auth_row_id;
                    auth_row_id = auth_row_id.wrapping_add(1);
                    actions = actions.child(components::with_focus(
                        div()
                            .id(("settings-agent-logout", row_id))
                            .h(px(theme.controls.min))
                            .px(px(theme.spacing.s2))
                            .flex()
                            .items_center()
                            .rounded(px(theme.radii.r8))
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.text)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.logout(session_id.clone(), cx);
                                    cx.notify();
                                });
                            }))
                            .child("Sign out"),
                        theme,
                    ));
                }
                let row_id = auth_row_id;
                auth_row_id = auth_row_id.wrapping_add(1);
                rows.push(
                    div()
                        .id(("settings-agent-session", row_id))
                        .flex()
                        .items_center()
                        .gap(px(theme.spacing.s3))
                        .px(px(theme.spacing.s2))
                        .py(px(theme.spacing.s1))
                        .border_b_1()
                        .border_color(theme.colors.border)
                        .child(
                            div()
                                .w(px(170.0))
                                .flex_shrink_0()
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.muted)
                                .child(format!("↳ {}", agent.name)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_size(px(theme.font.xs))
                                        .text_color(theme.colors.text)
                                        .child(headline),
                                )
                                .child(
                                    div()
                                        .text_size(px(theme.font.xs))
                                        .text_color(theme.colors.muted)
                                        .child(detail),
                                ),
                        )
                        .child(actions)
                        .into_any_element(),
                );
            }
        }

        card(theme)
            .child(components::section_label(theme, "AGENTS"))
            .child(hint(
                theme,
                "Read-only: the daemon reports availability, command and backend for each configured agent.",
            ))
            .when_some(notice, |card, notice| card.child(notice))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s3))
                    .px(px(theme.spacing.s2))
                    .py(px(2.0))
                    .text_size(px(theme.font.xs))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.colors.muted)
                    .child(div().w(px(170.0)).child("AGENT"))
                    .child(div().flex_1().child("COMMAND"))
                    .child(div().w(px(96.0)).child("BACKEND"))
                    .child(div().w(px(104.0)).child("STATUS"))
                    .child(div().w(px(76.0)).child("SESSIONS")),
            )
            .child(div().flex().flex_col().children(rows))
    }

    fn appearance_card(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let (theme_choice, density, focus) = {
            let state = self.state.read(cx);
            (
                state.theme_choice(),
                state.density(),
                state.settings().focus(),
            )
        };
        let theme_focus = focus == Some(SettingsFocus::Theme);
        let density_focus = focus == Some(SettingsFocus::Density);

        let theme_chips = div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .child(
                div()
                    .w(px(84.0))
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.muted)
                    .child("Theme"),
            )
            .children(
                [
                    crate::config::ThemeChoice::System,
                    crate::config::ThemeChoice::Dark,
                    crate::config::ThemeChoice::Light,
                ]
                .into_iter()
                .enumerate()
                .map(|(chip_index, choice)| {
                    let selected = theme_choice == choice;
                    div()
                        .id(("settings-theme", chip_index))
                        .h(px(theme.controls.min))
                        .px(px(theme.spacing.s3))
                        .flex()
                        .items_center()
                        .gap(px(theme.spacing.s1))
                        .rounded(px(theme.radii.r4))
                        .border_1()
                        .border_color(if selected {
                            theme.colors.accent
                        } else {
                            theme.colors.border
                        })
                        .when(selected, |chip| {
                            chip.bg(Hsla::from(theme.colors.accent).opacity(0.16))
                                .text_color(theme.colors.accent)
                        })
                        .when(!selected, |chip| {
                            chip.text_color(theme.colors.text)
                                .hover(|style| style.bg(theme.colors.bg))
                        })
                        .text_size(px(theme.font.sm))
                        .cursor_pointer()
                        .when(selected, |chip| chip.child("✓"))
                        .child(choice.label())
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.state.update(cx, |state, cx| {
                                state.focus_settings(Some(SettingsFocus::Theme));
                                state.set_theme_choice(choice, cx);
                                cx.notify();
                            });
                        }))
                }),
            );

        let density_chips = div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .child(
                div()
                    .w(px(84.0))
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.muted)
                    .child("Density"),
            )
            .children(
                [
                    crate::config::Density::Comfortable,
                    crate::config::Density::Compact,
                ]
                .into_iter()
                .enumerate()
                .map(|(chip_index, choice)| {
                    let selected = density == choice;
                    div()
                        .id(("settings-density", chip_index))
                        .h(px(theme.controls.min))
                        .px(px(theme.spacing.s3))
                        .flex()
                        .items_center()
                        .gap(px(theme.spacing.s1))
                        .rounded(px(theme.radii.r4))
                        .border_1()
                        .border_color(if selected {
                            theme.colors.accent
                        } else {
                            theme.colors.border
                        })
                        .when(selected, |chip| {
                            chip.bg(Hsla::from(theme.colors.accent).opacity(0.16))
                                .text_color(theme.colors.accent)
                        })
                        .when(!selected, |chip| {
                            chip.text_color(theme.colors.text)
                                .hover(|style| style.bg(theme.colors.bg))
                        })
                        .text_size(px(theme.font.sm))
                        .cursor_pointer()
                        .when(selected, |chip| chip.child("✓"))
                        .child(choice.label())
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.state.update(cx, |state, cx| {
                                state.focus_settings(Some(SettingsFocus::Density));
                                state.set_density(choice, cx);
                                cx.notify();
                            });
                        }))
                }),
            );

        card(theme)
            .child(components::section_label(theme, "APPEARANCE"))
            .child(theme_chips)
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(if theme_focus {
                        theme.colors.accent
                    } else {
                        theme.colors.muted
                    })
                    .child(if theme_focus {
                        "Enter cycles System → Dark → Light."
                    } else {
                        "System follows the operating system appearance. Changes apply immediately."
                    }),
            )
            .child(density_chips)
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(if density_focus {
                        theme.colors.accent
                    } else {
                        theme.colors.muted
                    })
                    .child(if density_focus {
                        "Enter toggles Comfortable ↔ Compact."
                    } else {
                        "Compact tightens spacing for short windows. Changes apply immediately."
                    }),
            )
    }

    fn about_card(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let (cwd, daemon_setting, endpoint, config_path) = {
            let state = self.state.read(cx);
            let cwd = state
                .last_ping()
                .and_then(|ping| ping.result.as_ref().ok())
                .map(|result| result.cwd.clone());
            (
                cwd,
                if state.connection().is_connected() {
                    "not reported by system.ping"
                } else {
                    "unavailable while disconnected"
                },
                state.config().endpoint.clone(),
                state.config().config_path.display().to_string(),
            )
        };

        let docs = components::with_focus(
            div()
                .id("settings-docs")
                .h(px(36.0))
                .px(px(theme.spacing.s3))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(theme.colors.border)
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.accent)
                .cursor_pointer()
                .track_focus(&self.docs_focus)
                .child("Open the client design notes")
                .on_click(cx.listener(|_this, _event, _window, cx| {
                    cx.open_url(DOCS_URL);
                })),
            theme,
        );

        card(theme)
            .child(components::section_label(theme, "ABOUT"))
            .child(value_row(
                theme,
                "Client version",
                env!("CARGO_PKG_VERSION"),
                true,
            ))
            .child(value_row(
                theme,
                "Daemon cwd",
                cwd.unwrap_or_else(|| "—".to_string()),
                true,
            ))
            .child(value_row(theme, "Daemon version", daemon_setting, false))
            .child(value_row(theme, "Endpoint", endpoint, true))
            .child(value_row(theme, "Settings file", config_path, true))
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child("No telemetry is collected or sent. The client only talks to the daemon you configure."),
            )
            .child(div().flex().items_center().gap(px(theme.spacing.s2)).child(docs))
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.state.read(cx).theme(window.appearance());

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.colors.bg)
            .child(self.header(theme))
            .child(
                div()
                    .id("settings-content")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .flex()
                    .flex_col()
                    .gap(px(theme.spacing.s4))
                    .p(px(theme.spacing.s4))
                    .child(self.connection_card(theme, cx))
                    .child(self.agents_card(theme, cx))
                    .child(self.appearance_card(theme, cx))
                    .child(self.about_card(theme, cx)),
            )
    }
}

fn card(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(theme.spacing.s2))
        .p(px(theme.spacing.s4))
        .rounded(px(theme.radii.r8))
        .border_1()
        .border_color(theme.colors.border)
        .bg(theme.colors.surface)
}

fn value_row(theme: &Theme, label: &str, value: impl Into<String>, mono: bool) -> Div {
    let value: String = value.into();
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(theme.spacing.s3))
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.muted)
                .child(label.to_string()),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .when(mono, |value| value.font_family("monospace"))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.text)
                .child(value),
        )
}

fn hint(theme: &Theme, text: &str) -> Div {
    div()
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child(text.to_string())
}
