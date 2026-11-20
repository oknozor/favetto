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
use crate::state::ClientState;
use crate::theme::Theme;

/// Where the About card points.
const DOCS_URL: &str = "https://github.com/oknozor/favetto/blob/main/docs/design/gpui-client.md";

/// The values one connection text field renders.
struct TextFieldSpec<'a> {
    id: &'static str,
    label: &'a str,
    value: &'a str,
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
}

impl SettingsView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self {
            state,
            scroll: ScrollHandle::new(),
            reconnect_focus: cx.focus_handle().tab_index(50).tab_stop(true),
            test_focus: cx.focus_handle().tab_index(51).tab_stop(true),
            save_focus: cx.focus_handle().tab_index(52).tab_stop(true),
            defaults_focus: cx.focus_handle().tab_index(53).tab_stop(true),
            docs_focus: cx.focus_handle().tab_index(54).tab_stop(true),
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
            value,
            placeholder,
            focused,
            focus,
        } = field;
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
                    .truncate()
                    .font_family("monospace")
                    .text_size(px(theme.font.sm))
                    .text_color(if value.is_empty() {
                        theme.colors.muted
                    } else {
                        theme.colors.text
                    })
                    .child(if value.is_empty() {
                        placeholder.to_string()
                    } else {
                        value.to_string()
                    }),
            )
            .when(focused, |field| {
                field.child(div().text_color(theme.colors.accent).child("▌"))
            })
    }

    fn connection_card(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let (endpoint, token, token_source, focus, normalized, test, save_error, connection) = {
            let state = self.state.read(cx);
            let settings = state.settings();
            (
                settings.draft_endpoint().to_string(),
                settings.draft_token().to_string(),
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
                value: &endpoint,
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
                value: &token,
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
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.warning)
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
            ConnectionTest::Failed { endpoint, message } => div()
                .flex()
                .items_start()
                .gap(px(theme.spacing.s1))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.danger)
                .child(format!("⚠ {endpoint} failed: {message}"))
                .into_any_element(),
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
            .child(test_line)
            .when_some(save_error, |card, error| {
                card.child(
                    div()
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.danger)
                        .child(error),
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
        let agents = self.state.read(cx).agent_roster().to_vec();
        let rows: Vec<_> = if agents.is_empty() {
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

        card(theme)
            .child(components::section_label(theme, "AGENTS"))
            .child(hint(
                theme,
                "Read-only: the daemon reports availability, command and backend for each configured agent.",
            ))
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
                        .h(px(32.0))
                        .px(px(theme.spacing.s3))
                        .flex()
                        .items_center()
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
                        .h(px(32.0))
                        .px(px(theme.spacing.s3))
                        .flex()
                        .items_center()
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
