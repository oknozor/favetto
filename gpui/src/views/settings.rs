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

use crate::agent_auth;
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
    daemon_save_focus: FocusHandle,
    daemon_reload_focus: FocusHandle,
    daemon_reset_focus: FocusHandle,
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
            daemon_save_focus: cx.focus_handle().tab_index(27).tab_stop(true),
            daemon_reload_focus: cx.focus_handle().tab_index(28).tab_stop(true),
            daemon_reset_focus: cx.focus_handle().tab_index(29).tab_stop(true),
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

    fn daemon_text_control(
        &self,
        theme: &Theme,
        control: usize,
        focused: bool,
        placeholder: &str,
        masked: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(("settings-daemon-control", control))
            .flex_1()
            .min_w_0()
            .h(px(34.0))
            .px(px(theme.spacing.s2))
            .flex()
            .items_center()
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(if focused {
                theme.colors.accent
            } else {
                theme.colors.border
            })
            .bg(theme.colors.bg)
            .cursor_text()
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.focus_settings(Some(SettingsFocus::DaemonControl(control)));
                    cx.notify();
                });
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family("monospace")
                    .text_size(px(theme.font.sm))
                    .child(
                        state_text_element(
                            &self.state,
                            TextTarget::SettingsDaemonControl(control),
                            theme,
                            cx,
                        )
                        .placeholder(placeholder)
                        .masked(masked),
                    ),
            )
    }

    fn daemon_small_button(
        &self,
        theme: &Theme,
        id: impl Into<gpui::ElementId>,
        label: impl Into<String>,
        cx: &mut Context<Self>,
        action: impl Fn(&mut ClientState) + 'static,
    ) -> Stateful<Div> {
        components::with_focus(
            div()
                .id(id)
                .h(px(theme.controls.min))
                .px(px(theme.spacing.s2))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s1))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(theme.colors.border)
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .hover(|style| style.bg(theme.colors.bg))
                .child(label.into())
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        action(state);
                        cx.notify();
                    });
                })),
            theme,
        )
    }

    fn daemon_enum_chips(
        &self,
        theme: &Theme,
        field: usize,
        choices: &[crate::daemon_form::EnumChoice],
        selected: Option<&str>,
        optional: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut chips = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(theme.spacing.s1));
        if optional {
            let is_selected = selected.is_none();
            chips = chips.child(self.daemon_choice_chip(
                theme,
                element_id(format!("daemon-enum-{field}-unset")),
                "Unset / default",
                is_selected,
                cx,
                move |state| state.daemon_config_mut().set_enum(field, None),
            ));
        }
        for (index, choice) in choices.iter().enumerate() {
            let choice_value = choice.value.clone();
            let is_selected = selected == Some(choice.value.as_str());
            chips = chips.child(self.daemon_choice_chip(
                theme,
                element_id(format!("daemon-enum-{field}-{index}")),
                choice.label().to_string(),
                is_selected,
                cx,
                move |state| {
                    state
                        .daemon_config_mut()
                        .set_enum(field, Some(choice_value.clone()))
                },
            ));
        }
        chips
    }

    fn daemon_choice_chip(
        &self,
        theme: &Theme,
        id: impl Into<gpui::ElementId>,
        label: impl Into<String>,
        selected: bool,
        cx: &mut Context<Self>,
        action: impl Fn(&mut ClientState) + 'static,
    ) -> Stateful<Div> {
        let mut chip = div()
            .id(id)
            .h(px(theme.controls.min))
            .px(px(theme.spacing.s2))
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
            .text_size(px(theme.font.xs))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    action(state);
                    cx.notify();
                });
            }));
        if selected {
            chip = chip
                .bg(Hsla::from(theme.colors.accent).opacity(0.16))
                .text_color(theme.colors.accent)
                .child("✓");
        } else {
            chip = chip
                .text_color(theme.colors.text)
                .hover(|style| style.bg(theme.colors.bg));
        }
        chip.child(label.into())
    }

    fn daemon_field_row(
        &self,
        theme: &Theme,
        form: &crate::daemon_form::ConfigForm,
        field_index: usize,
        overrides: &[favetto_wire::rpc::ConfigOverride],
        focus: Option<SettingsFocus>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        use crate::daemon_form::{ControlRef, FieldKind};

        let field = &form.fields()[field_index];
        let controls = form.field_controls(field_index);
        let overridden = overrides.iter().any(|entry| {
            entry.key == field.path
                || field.path.starts_with(&format!("{}.", entry.key))
                || entry.key.starts_with(&format!("{}.", field.path))
        });

        let mut header = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap(px(theme.spacing.s1))
            .child(
                div()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(field.label.clone()),
            );
        if field.required {
            header = header.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.warning)
                    .child("required"),
            );
        }
        if let Some(default) = &field.default {
            header = header.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(format!("default: {default}")),
            );
        }
        if overridden {
            header = header.child(
                div()
                    .px(px(theme.spacing.s1))
                    .rounded(px(theme.radii.r4))
                    .border_1()
                    .border_color(theme.colors.warning)
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.warning)
                    .child("FAVETTO_* override"),
            );
        }
        if field.boot_only {
            header = header.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.warning)
                    .child("boot-only"),
            );
        }
        header = header.child(div().flex_1());
        if field.is_present() {
            header = header.child(self.daemon_small_button(
                theme,
                ("daemon-reset-field", field_index),
                "Reset",
                cx,
                move |state| state.daemon_config_mut().reset_field(field_index),
            ));
        }

        let mut row = div()
            .id(("daemon-field", field_index))
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s1))
            .px(px(theme.spacing.s2))
            .py(px(theme.spacing.s1))
            .rounded(px(theme.radii.r4))
            .child(header);
        if !field.help.is_empty() {
            row = row.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(field.help.clone()),
            );
        }

        let placeholder = match &field.default {
            Some(default) => format!("Unset (default: {default})"),
            None => "Unset".to_string(),
        };
        let control_row = match &field.kind {
            FieldKind::Text { numeric: true, .. } => {
                let unit = field_unit(&field.path);
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(
                        self.daemon_text_control(
                            theme,
                            controls.first().map(|(index, _)| *index).unwrap_or(0),
                            focus
                                == controls
                                    .first()
                                    .map(|(index, _)| SettingsFocus::DaemonControl(*index)),
                            &placeholder,
                            false,
                            cx,
                        ),
                    )
                    .when_some(unit, |row, unit| {
                        row.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.muted)
                                .child(unit),
                        )
                    })
                    .into_any_element()
            }
            FieldKind::Text { .. } => self
                .daemon_text_control(
                    theme,
                    controls.first().map(|(index, _)| *index).unwrap_or(0),
                    focus
                        == controls
                            .first()
                            .map(|(index, _)| SettingsFocus::DaemonControl(*index)),
                    &placeholder,
                    false,
                    cx,
                )
                .into_any_element(),
            FieldKind::Bool { value, present } => {
                let control = controls.first().map(|(index, _)| *index).unwrap_or(0);
                let label = if *present {
                    if *value {
                        "Enabled"
                    } else {
                        "Disabled"
                    }
                } else {
                    "Use default"
                };
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(
                        div()
                            .id(("daemon-bool", field_index))
                            .h(px(theme.controls.min))
                            .px(px(theme.spacing.s2))
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .rounded(px(theme.radii.r4))
                            .border_1()
                            .border_color(if focus == Some(SettingsFocus::DaemonControl(control)) {
                                theme.colors.accent
                            } else {
                                theme.colors.border
                            })
                            .bg(theme.colors.bg)
                            .cursor_pointer()
                            .text_size(px(theme.font.sm))
                            .text_color(theme.colors.text)
                            .child(if *present && *value { "☑" } else { "☐" })
                            .child(label)
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.focus_settings(Some(SettingsFocus::DaemonControl(
                                        control,
                                    )));
                                    let current = state
                                        .daemon_config()
                                        .form()
                                        .and_then(|form| form.field(field_index))
                                        .is_some_and(|field| {
                                            matches!(
                                                field.kind,
                                                FieldKind::Bool { value: true, .. }
                                            )
                                        });
                                    state.daemon_config_mut().set_bool(field_index, !current);
                                    cx.notify();
                                });
                            })),
                    )
                    .into_any_element()
            }
            FieldKind::Enum {
                choices,
                selected,
                optional,
            } => self
                .daemon_enum_chips(
                    theme,
                    field_index,
                    choices,
                    selected.as_deref(),
                    *optional,
                    cx,
                )
                .into_any_element(),
            FieldKind::List { rows, choices, .. } => {
                let mut list = div().flex().flex_col().gap(px(theme.spacing.s1));
                if let Some(choices) = choices {
                    for (row, row_input) in rows.iter().enumerate() {
                        let selected = row_input.value().to_string();
                        let selected = if selected.is_empty() {
                            choices.first().map(|choice| choice.value.clone())
                        } else {
                            Some(selected)
                        };
                        let mut choices_row = div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(theme.spacing.s1));
                        for (choice_index, choice) in choices.iter().enumerate() {
                            let value = choice.value.clone();
                            choices_row = choices_row.child(self.daemon_choice_chip(
                                theme,
                                element_id(format!(
                                    "daemon-list-choice-{field_index}-{row}-{choice_index}"
                                )),
                                choice.label().to_string(),
                                selected.as_deref() == Some(value.as_str()),
                                cx,
                                move |state| {
                                    state.daemon_config_mut().set_list_choice(
                                        field_index,
                                        row,
                                        value.clone(),
                                    );
                                },
                            ));
                        }
                        list = list.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(theme.spacing.s2))
                                .child(choices_row)
                                .child(self.daemon_small_button(
                                    theme,
                                    element_id(format!("daemon-list-remove-{field_index}-{row}")),
                                    "Remove",
                                    cx,
                                    move |state| {
                                        state.daemon_config_mut().remove_list_item(field_index, row)
                                    },
                                )),
                        );
                    }
                } else {
                    for (row, input) in rows.iter().enumerate() {
                        let control = controls
                            .iter()
                            .find(|(_, control)| {
                                matches!(control, ControlRef::ListItem(_, index) if *index == row)
                            })
                            .map(|(index, _)| *index)
                            .unwrap_or(0);
                        list = list.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(theme.spacing.s2))
                                .child(self.daemon_text_control(
                                    theme,
                                    control,
                                    focus == Some(SettingsFocus::DaemonControl(control)),
                                    "Value",
                                    false,
                                    cx,
                                ))
                                .child(self.daemon_small_button(
                                    theme,
                                    element_id(format!("daemon-list-remove-{field_index}-{row}")),
                                    "Remove",
                                    cx,
                                    move |state| {
                                        state.daemon_config_mut().remove_list_item(field_index, row)
                                    },
                                )),
                        );
                        let _ = input;
                    }
                }
                list = list.child(self.daemon_small_button(
                    theme,
                    ("daemon-list-add", field_index),
                    "Add entry",
                    cx,
                    move |state| state.daemon_config_mut().add_list_item(field_index),
                ));
                list.into_any_element()
            }
            FieldKind::Map { rows, secret } => {
                let secret_value = *secret;
                let mut map = div().flex().flex_col().gap(px(theme.spacing.s1));
                for (row, entry) in rows.iter().enumerate() {
                    let key_control = controls
                        .iter()
                        .find(|(_, control)| {
                            matches!(control, ControlRef::MapKey(_, index) if *index == row)
                        })
                        .map(|(index, _)| *index)
                        .unwrap_or(0);
                    let value_control = controls
                        .iter()
                        .find(|(_, control)| {
                            matches!(control, ControlRef::MapValue(_, index) if *index == row)
                        })
                        .map(|(index, _)| *index)
                        .unwrap_or(0);
                    let value_placeholder = if secret_value && entry.unchanged {
                        "Unchanged secret — type to replace"
                    } else if secret_value {
                        "Secret value"
                    } else {
                        "Value"
                    };
                    map = map.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .child(
                                div()
                                    .w(px(180.0))
                                    .flex_shrink_0()
                                    .flex()
                                    .flex_col()
                                    .child(self.daemon_text_control(
                                        theme,
                                        key_control,
                                        focus == Some(SettingsFocus::DaemonControl(key_control)),
                                        "Key",
                                        false,
                                        cx,
                                    ))
                                    .when_some(entry.key_error.clone(), |cell, error| {
                                        cell.child(
                                            div()
                                                .text_size(px(theme.font.xs))
                                                .text_color(theme.colors.danger)
                                                .child(error),
                                        )
                                    }),
                            )
                            .child(self.daemon_text_control(
                                theme,
                                value_control,
                                focus == Some(SettingsFocus::DaemonControl(value_control)),
                                value_placeholder,
                                secret_value,
                                cx,
                            ))
                            .child(self.daemon_small_button(
                                theme,
                                element_id(format!("daemon-map-clear-{field_index}-{row}")),
                                if secret_value { "Clear" } else { "Remove" },
                                cx,
                                move |state| {
                                    if secret_value {
                                        state.daemon_config_mut().clear_secret(field_index, row)
                                    } else {
                                        state.daemon_config_mut().remove_map_row(field_index, row)
                                    }
                                },
                            )),
                    );
                }
                map = map.child(self.daemon_small_button(
                    theme,
                    ("daemon-map-add", field_index),
                    "Add entry",
                    cx,
                    move |state| state.daemon_config_mut().add_map_row(field_index),
                ));
                map.into_any_element()
            }
        };
        row = row.child(control_row);
        if let Some(error) = &field.error {
            row = row.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.danger)
                    .child(format!("⚠ {error}")),
            );
        }
        row
    }

    fn daemon_form_view(
        &self,
        theme: &Theme,
        form: &crate::daemon_form::ConfigForm,
        overrides: &[favetto_wire::rpc::ConfigOverride],
        focus: Option<SettingsFocus>,
        cx: &mut Context<Self>,
    ) -> Div {
        use crate::daemon_form::LayoutItem;

        let mut body = div().flex().flex_col().gap(px(theme.spacing.s2));
        for item in form.layout() {
            match item {
                LayoutItem::Section(section) => {
                    body = body.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .pt(px(theme.spacing.s2))
                            .child(components::section_label(theme, section.label()))
                            .when_some(section.note(), |section, note| {
                                section.child(
                                    div()
                                        .text_size(px(theme.font.xs))
                                        .text_color(theme.colors.muted)
                                        .child(note),
                                )
                            }),
                    );
                }
                LayoutItem::Group(index) => {
                    let group = &form.groups()[*index];
                    let mut header = div()
                        .id(("daemon-group", *index))
                        .flex()
                        .items_center()
                        .flex_wrap()
                        .gap(px(theme.spacing.s2))
                        .py(px(2.0))
                        .border_b_1()
                        .border_color(theme.colors.border);
                    if !group.title.is_empty() {
                        header = header.child(
                            div()
                                .text_size(px(theme.font.xs))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.colors.muted)
                                .child(group.title.clone()),
                        );
                    }
                    header = header.child(div().flex_1());
                    if let Some(remove) = group.remove.clone() {
                        header = header.child(self.daemon_small_button(
                            theme,
                            ("daemon-remove", *index),
                            "Remove",
                            cx,
                            move |state| {
                                state.daemon_config_mut().remove(&remove);
                                state.focus_settings(None);
                            },
                        ));
                    }
                    if let Some(add) = group.add.clone() {
                        header = header.child(self.daemon_small_button(
                            theme,
                            ("daemon-add", *index),
                            add.label(),
                            cx,
                            move |state| {
                                state.daemon_config_mut().add(&add);
                                state.focus_settings(None);
                            },
                        ));
                    }
                    body = body.child(header);
                }
                LayoutItem::Field(index) => {
                    body = body
                        .child(self.daemon_field_row(theme, form, *index, overrides, focus, cx));
                }
                LayoutItem::EndGroup(_) => {}
            }
        }
        body
    }

    fn daemon_config_card(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let (
            path,
            exists,
            status,
            errors,
            warnings,
            overrides,
            restart,
            live,
            dirty,
            saving,
            loading,
            load_error,
            focus,
            advanced,
            form,
        ) = {
            let state = self.state.read(cx);
            let config = state.daemon_config();
            (
                config.path().map(str::to_string),
                config.exists(),
                state.daemon_config_status(),
                config.errors().to_vec(),
                config.warnings().to_vec(),
                config.overrides().to_vec(),
                config.restart_required().to_vec(),
                config.live_sections().to_vec(),
                config.is_dirty(),
                config.is_saving(),
                config.is_loading(),
                config.load_error().map(str::to_string),
                state.settings().focus(),
                config.is_advanced(),
                config.form().cloned(),
            )
        };
        let can_save = self.state.read(cx).daemon_config().can_save();
        let status_color = match status {
            crate::daemon_config::DaemonConfigStatus::Invalid => theme.colors.danger,
            crate::daemon_config::DaemonConfigStatus::Dirty
            | crate::daemon_config::DaemonConfigStatus::Loading
            | crate::daemon_config::DaemonConfigStatus::Saving => theme.colors.warning,
            crate::daemon_config::DaemonConfigStatus::Saved => theme.colors.success,
            crate::daemon_config::DaemonConfigStatus::Idle => theme.colors.muted,
        };

        let save = components::with_focus(
            div()
                .id("settings-daemon-save")
                .h(px(36.0))
                .px(px(theme.spacing.s3))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .when(can_save, |button| {
                    button
                        .bg(Hsla::from(theme.colors.accent).opacity(0.20))
                        .text_color(theme.colors.accent)
                        .cursor_pointer()
                })
                .when(!can_save, |button| {
                    button
                        .border_1()
                        .border_color(theme.colors.border)
                        .text_color(theme.colors.muted)
                        .opacity(0.6)
                        .cursor_default()
                })
                .border_color(if focus == Some(SettingsFocus::DaemonSave) {
                    theme.colors.accent
                } else {
                    theme.colors.border
                })
                .text_size(px(theme.font.sm))
                .track_focus(&self.daemon_save_focus)
                .child(if saving {
                    "Saving…".to_string()
                } else {
                    "Save daemon config".to_string()
                })
                .child(components::keycap(theme, "⌘↵"))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.focus_settings(Some(SettingsFocus::DaemonSave));
                        state.save_daemon_config(cx);
                        cx.notify();
                    });
                })),
            theme,
        );

        let reload = components::with_focus(
            div()
                .id("settings-daemon-reload")
                .h(px(36.0))
                .px(px(theme.spacing.s3))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s1))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(if focus == Some(SettingsFocus::DaemonReload) {
                    theme.colors.accent
                } else {
                    theme.colors.border
                })
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.daemon_reload_focus)
                .hover(|style| style.bg(theme.colors.bg))
                .child(if loading { "Loading…" } else { "Reload" })
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.focus_settings(Some(SettingsFocus::DaemonReload));
                        state.reload_daemon_config(cx);
                        cx.notify();
                    });
                })),
            theme,
        );

        let reset = components::with_focus(
            div()
                .id("settings-daemon-reset")
                .h(px(36.0))
                .px(px(theme.spacing.s3))
                .flex()
                .items_center()
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(theme.colors.border)
                .text_size(px(theme.font.sm))
                .text_color(if dirty {
                    theme.colors.text
                } else {
                    theme.colors.muted
                })
                .when(dirty, |button| {
                    button
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.colors.bg))
                })
                .track_focus(&self.daemon_reset_focus)
                .child("Discard edits")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.reset_daemon_config();
                        state.focus_settings(None);
                        cx.notify();
                    });
                })),
            theme,
        );

        let advanced_toggle = self.daemon_small_button(
            theme,
            "settings-daemon-advanced",
            if advanced {
                "Back to form"
            } else {
                "Advanced / raw TOML"
            },
            cx,
            |state| {
                let next = !state.daemon_config().is_advanced();
                state.daemon_config_mut().set_advanced(next);
                let advanced = state.daemon_config().is_advanced();
                state.focus_settings(Some(if advanced {
                    SettingsFocus::DaemonRaw
                } else if state.daemon_config().control_count() > 0 {
                    SettingsFocus::DaemonControl(0)
                } else {
                    SettingsFocus::DaemonForm
                }));
            },
        );

        let editor = div()
            .id("settings-daemon-config-editor")
            .flex()
            .flex_col()
            .min_h(px(220.0))
            .p(px(theme.spacing.s3))
            .rounded(px(theme.radii.r4))
            .border_2()
            .border_color(if focus == Some(SettingsFocus::DaemonRaw) {
                theme.colors.accent
            } else {
                theme.colors.border
            })
            .bg(theme.colors.bg)
            .cursor_text()
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.focus_settings(Some(SettingsFocus::DaemonRaw));
                    cx.notify();
                });
            }))
            .child(
                div()
                    .font_family("monospace")
                    .text_size(px(theme.font.sm))
                    .child(
                        state_text_element(
                            &self.state,
                            TextTarget::SettingsDaemonRaw,
                            theme,
                            cx,
                        )
                        .multiline(true)
                        .max_visible_lines(24)
                        .placeholder(
                            "# ~/.config/favetto/config.toml\n# A missing file is valid: defaults apply.",
                        ),
                    ),
            );

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for error in &errors {
            rows.push(
                div()
                    .flex()
                    .items_start()
                    .gap(px(theme.spacing.s2))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.danger)
                    .child("⚠")
                    .child(div().flex_1().min_w(px(0.0)).child(error.clone()))
                    .into_any_element(),
            );
        }
        if let Some(load_error) = load_error {
            rows.push(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.danger)
                    .child(format!("Config load failed: {load_error}"))
                    .into_any_element(),
            );
        }
        for warning in &warnings {
            rows.push(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.warning)
                    .child(warning.clone())
                    .into_any_element(),
            );
        }
        if !restart.is_empty() {
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap(px(theme.spacing.s1))
                    .text_size(px(theme.font.xs))
                    .child(
                        div()
                            .text_color(theme.colors.warning)
                            .child("Restart required:"),
                    )
                    .children(restart.iter().map(|section| {
                        div()
                            .px(px(theme.spacing.s2))
                            .py(px(1.0))
                            .rounded(px(theme.radii.r4))
                            .border_1()
                            .border_color(theme.colors.border)
                            .text_color(theme.colors.warning)
                            .child(section.clone())
                    }))
                    .into_any_element(),
            );
        }
        if !live.is_empty() {
            rows.push(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.success)
                    .child(format!("Live without restart: {}", live.join(", ")))
                    .into_any_element(),
            );
        }
        if !overrides.is_empty() {
            rows.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child("Environment overrides shadow the file:")
                    .children(overrides.iter().map(|entry| {
                        div()
                            .font_family("monospace")
                            .child(format!("{} ({})", entry.key, entry.source))
                    }))
                    .into_any_element(),
            );
        }

        let form_body = form
            .as_ref()
            .map(|form| self.daemon_form_view(theme, form, &overrides, focus, cx));

        card(theme)
            .child(components::section_label(theme, "DAEMON / GLOBAL CONFIG"))
            .child(hint(
                theme,
                "Edit the daemon's config.toml with typed controls. Values are patched into the \
                 loaded file, so comments and unknown keys survive; Save still validates, checks \
                 the revision and writes atomically. Use the advanced view for comments, unknown \
                 keys, client-side [tui] settings and malformed-file repair.",
            ))
            .child(value_row(
                theme,
                "Daemon config file",
                path.unwrap_or_else(|| "not loaded".to_string()),
                true,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap(px(theme.spacing.s2))
                    .text_size(px(theme.font.xs))
                    .child(
                        div()
                            .text_color(status_color)
                            .child(format!("● {}", status.label())),
                    )
                    .child(div().text_color(theme.colors.muted).child(if exists {
                        "file present"
                    } else {
                        "missing file — defaults apply"
                    }))
                    .child(div().flex_1())
                    .child(advanced_toggle),
            )
            .when(advanced || form_body.is_none(), |card| card.child(editor))
            .when(!advanced, |card| match form_body {
                Some(form_body) => card.child(form_body),
                None => card,
            })
            .child(hint(
                theme,
                "The [repos] and [mcp] sections only seed their registries at startup; use the \
                 repos.*/mcp.* RPCs for live changes. Changing daemon listen/socket/tasks_dir/\
                 data_dir, auth.require_tls or an agent command can make the daemon unreachable or \
                 launch a different executable after restart. [tui] is client-side and lives in \
                 the advanced view.",
            ))
            .when(!rows.is_empty(), |card| {
                card.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(theme.spacing.s1))
                        .children(rows),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .pt(px(theme.spacing.s1))
                    .child(reload)
                    .child(reset)
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

        // ACP authentication is session-scoped. Settings only reports the
        // current status: sign-in and sign-out live in the session's task
        // details or Agent panel, where the daemon accepts them.
        let mut auth_row_id = 0usize;
        for agent in &agents {
            for session in &agent.sessions {
                let Some(auth) = &session.auth else {
                    continue;
                };
                let row = agent_auth::settings_row(auth);
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
                                        .child(row.headline),
                                )
                                .child(
                                    div()
                                        .text_size(px(theme.font.xs))
                                        .text_color(theme.colors.muted)
                                        .child(row.detail),
                                ),
                        )
                        .into_any_element(),
                );
            }
        }

        card(theme)
            .child(components::section_label(theme, "AGENTS"))
            .child(hint(
                theme,
                "Read-only: the daemon reports availability, command and backend for each configured agent. Session sign-in happens in the task details or Agent panel.",
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
        let (cwd, daemon_setting, endpoint, client_config_path, daemon_config_path) = {
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
                state
                    .daemon_config()
                    .path()
                    .map(str::to_string)
                    .unwrap_or_else(|| "unavailable while disconnected".to_string()),
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
            .child(value_row(
                theme,
                "Client settings (gpui.toml)",
                client_config_path,
                true,
            ))
            .child(value_row(
                theme,
                "Daemon config (config.toml)",
                daemon_config_path,
                true,
            ))
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
                    .child(self.daemon_config_card(theme, cx))
                    .child(self.agents_card(theme, cx))
                    .child(self.appearance_card(theme, cx))
                    .child(self.about_card(theme, cx)),
            )
    }
}

fn element_id(value: String) -> gpui::ElementId {
    gpui::ElementId::Name(gpui::SharedString::new(value))
}

fn field_unit(path: &str) -> Option<&'static str> {
    if path.ends_with("_ms") {
        Some("milliseconds")
    } else if path.ends_with("_secs") {
        Some("seconds")
    } else if path.ends_with("_bytes") {
        Some("bytes")
    } else if path.ends_with("days") {
        Some("days")
    } else if path.ends_with("_worktrees") || path.ends_with("min_tasks") {
        Some("count")
    } else {
        None
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
