//! Settings: connection, agents and appearance.
//!
//! Step 03 keeps this read-only except for Reconnect; the editing form lands
//! with the schedules/settings step. Showing the effective endpoint and token
//! source here already removes the "which daemon am I talking to?" question.

use gpui::{
    div, prelude::*, px, Context, Div, Entity, FocusHandle, Hsla, IntoElement, Render, Window,
    WindowAppearance,
};

use crate::components;
use crate::routes::Route;
use crate::state::ClientState;
use crate::theme::Theme;

/// The Settings surface.
pub struct SettingsView {
    state: Entity<ClientState>,
    reconnect_focus: FocusHandle,
}

impl SettingsView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self {
            state,
            reconnect_focus: cx.focus_handle().tab_index(20).tab_stop(true),
        }
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        let (endpoint, token_source, connection, error) = {
            let state = self.state.read(cx);
            (
                state.config().endpoint.clone(),
                state.config().token_source.label(),
                state.connection().clone(),
                state.last_error().map(str::to_string),
            )
        };

        let reconnect_state = self.state.clone();
        let reconnect = components::with_focus(
            div()
                .id("settings-reconnect")
                .h(px(36.0))
                .px(px(theme.spacing.s3))
                .flex()
                .items_center()
                .rounded(px(theme.radii.r4))
                .bg(Hsla::from(theme.colors.accent).opacity(0.14))
                .text_color(theme.colors.text)
                .text_size(px(theme.font.sm))
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

        let connection_card = card(theme)
            .child(components::section_label(theme, "CONNECTION"))
            .child(value_row(theme, "Endpoint", endpoint, true))
            .child(value_row(theme, "Token source", token_source, true))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(theme.font.sm))
                            .text_color(theme.colors.muted)
                            .child("Status"),
                    )
                    .child(components::connection_pill(theme, &connection)),
            )
            .when_some(error, |card, error| {
                card.child(
                    div()
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .mt(px(theme.spacing.s1))
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(reconnect)
                    .child(
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child("Reconnects the event stream and re-runs the ping."),
                    ),
            );

        let agents_card = card(theme)
            .child(components::section_label(theme, "AGENTS"))
            .child(
                div()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child("Agent roster"),
            )
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(
                        "Agents reported by the daemon appear here with their provider, model and live session count.",
                    ),
            );

        let appearance_card = card(theme)
            .child(components::section_label(theme, "APPEARANCE"))
            .child(value_row(
                theme,
                "Theme",
                appearance_label(window.appearance()),
                false,
            ))
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child("Dark and light follow the operating system appearance."),
            );

        div()
            .id("settings-content")
            .size_full()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s4))
            .p(px(theme.spacing.s6))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(theme.spacing.s1))
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child(Route::Settings.label()),
                    )
                    .child(
                        div()
                            .text_size(px(theme.font.sm))
                            .text_color(theme.colors.muted)
                            .child(Route::Settings.empty_body()),
                    ),
            )
            .child(connection_card)
            .child(agents_card)
            .child(appearance_card)
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

fn appearance_label(appearance: WindowAppearance) -> &'static str {
    match appearance {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => "Dark (system)",
        WindowAppearance::Light | WindowAppearance::VibrantLight => "Light (system)",
    }
}
