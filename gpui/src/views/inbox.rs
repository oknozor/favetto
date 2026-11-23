//! Inbox: the "what needs me now?" landing surface.
//!
//! Two groups, in this order (docs/design/gpui-client.md §1, Miller's Law):
//!
//! 1. **Needs you** — blocked sessions plus failures from the last 24 h, oldest
//!    waiting first so no prompt can starve (a fair queue).
//! 2. **Recent** — the newest runs, one line each.
//!
//! The aggregation lives in [`crate::inbox`] (pure and unit-tested); this file
//! only renders it. The scroll handle is owned by the view and persists across
//! refreshes, so a live update never yanks the reader back to the top (docs §2,
//! Working Memory).
//!
//! Keyboard is owned by the shell (`↑`/`↓`, `Enter`, `r`); rows are clickable
//! for the same primary action — open the run (docs §1, "One primary action").

use chrono::Utc;
use gpui::{
    div, prelude::*, px, Context, Div, Entity, Hsla, IntoElement, Render, Rgba, ScrollHandle,
    Stateful, Window,
};

use crate::components;
use crate::inbox::{self, InboxKey, NeedsRow, RecentRow, StatusKind};
use crate::routes::Route;
use crate::state::{ClientState, SnapshotStatus};
use crate::theme::Theme;

/// The Inbox surface.
pub struct InboxView {
    state: Entity<ClientState>,
    /// Persisted across renders so live row updates keep the scroll position.
    scroll: ScrollHandle,
    retry_focus: gpui::FocusHandle,
    error_copy_focus: gpui::FocusHandle,
}

impl InboxView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self {
            state,
            scroll: ScrollHandle::new(),
            retry_focus: cx.focus_handle().tab_index(20).tab_stop(true),
            error_copy_focus: cx.focus_handle().tab_index(21).tab_stop(true),
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
                    .child(components::section_label(theme, "INBOX"))
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child("What needs you now"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(components::keycap(theme, "↑↓"))
                    .child(
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child("move"),
                    )
                    .child(components::keycap(theme, "↵"))
                    .child(
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child("open"),
                    )
                    .child(components::keycap(theme, "r"))
                    .child(
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child("refresh"),
                    ),
            )
    }

    fn retry_button(&self, theme: &Theme, cx: &mut Context<Self>) -> Stateful<Div> {
        components::with_focus(
            div()
                .id("inbox-retry")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .h(px(theme.controls.primary))
                .px(px(theme.spacing.s3))
                .rounded(px(theme.radii.r8))
                .bg(Hsla::from(theme.colors.accent).opacity(0.16))
                .text_color(theme.colors.accent)
                .text_size(px(theme.font.sm))
                .cursor_pointer()
                .track_focus(&self.retry_focus)
                .child("Retry")
                .child(components::keycap(theme, "r"))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.refresh_snapshot();
                        cx.notify();
                    });
                })),
            theme,
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

    fn loading_body(&self, theme: &Theme) -> Div {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s4))
            .p(px(theme.spacing.s4))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(components::section_label(theme, "NEEDS YOU"))
                    .child(
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child("Loading…"),
                    ),
            )
            .children((0..3).map(|index| skeleton_row(theme, 160.0 + index as f32 * 32.0)))
            .child(div().child(components::section_label(theme, "RECENT")))
            .children((0..5).map(|index| skeleton_row(theme, 220.0 + index as f32 * 24.0)))
    }

    fn error_body(&self, theme: &Theme, message: &str, cx: &mut Context<Self>) -> Div {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(theme.spacing.s3))
            .p(px(theme.spacing.s6))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(48.0))
                    .h(px(48.0))
                    .rounded(px(theme.radii.r12))
                    .border_1()
                    .border_color(Hsla::from(theme.colors.danger).opacity(0.35))
                    .bg(Hsla::from(theme.colors.danger).opacity(0.12))
                    .text_color(theme.colors.danger)
                    .text_size(px(theme.font.xl))
                    .child("⚠"),
            )
            .child(components::section_label(theme, "INBOX"))
            .child(
                div()
                    .text_size(px(theme.font.lg))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text)
                    .child("The inbox could not load"),
            )
            .child(
                div()
                    .max_w(px(520.0))
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.muted)
                    .text_center()
                    .child(message.to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(self.error_copy_button(theme, message, "inbox-error-copy", cx))
                    .child(self.retry_button(theme, cx)),
            )
            .child(
                div()
                    .max_w(px(480.0))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .text_center()
                    .child(
                        "Check that the daemon is running and the token is readable; `r` retries without leaving the surface."
                            .to_string(),
                    ),
            )
    }

    fn error_banner(&self, theme: &Theme, message: &str, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s3))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s2))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(Hsla::from(theme.colors.danger).opacity(0.45))
            .bg(Hsla::from(theme.colors.danger).opacity(0.10))
            .child(
                div()
                    .text_color(theme.colors.danger)
                    .text_size(px(theme.font.xs))
                    .child("⚠"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.text)
                    .child(format!("Refresh failed: {message}")),
            )
            .child(self.error_copy_button(theme, message, "inbox-error-copy-banner", cx))
            .child(self.retry_button(theme, cx))
    }

    fn needs_section(
        &self,
        theme: &Theme,
        rows: &[NeedsRow],
        selected: Option<&InboxKey>,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut section = div().flex().flex_col().gap(px(theme.spacing.s2)).child(
            div()
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .child(components::section_label(theme, "NEEDS YOU"))
                .when(!rows.is_empty(), |header| {
                    header.child(count_pill(theme, rows.len(), theme.colors.warning))
                })
                .child(div().flex_1())
                .when(!rows.is_empty(), |header| {
                    header.child(
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child("oldest first"),
                    )
                }),
        );

        if rows.is_empty() {
            section = section.child(all_clear(theme));
        } else {
            for (index, row) in rows.iter().enumerate() {
                section = section.child(self.needs_row(
                    theme,
                    index,
                    row,
                    selected.is_some_and(|selected| selected == &row.key),
                    cx,
                ));
            }
        }
        section
    }

    fn needs_row(
        &self,
        theme: &Theme,
        index: usize,
        row: &NeedsRow,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let status_color = status_color(theme, row.status);
        let awaiting = row.status == StatusKind::AwaitingInput;
        // Von Restorff: awaiting rows carry the accent stripe + icon; failures
        // use the danger stripe. The label always accompanies the color.
        let stripe = if awaiting {
            theme.colors.accent
        } else if row.status == StatusKind::Failed {
            theme.colors.danger
        } else {
            theme.colors.muted
        };
        let elapsed = inbox::elapsed_since(row.since, Utc::now());
        let click_key = row.key.clone();

        div()
            .id(("inbox-need", index))
            .flex()
            .items_start()
            .gap(px(theme.spacing.s3))
            .w_full()
            .min_h(px(44.0))
            .p(px(theme.spacing.s3))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(if selected {
                theme.colors.accent
            } else {
                theme.colors.border
            })
            .bg(if selected {
                Hsla::from(theme.colors.accent).opacity(0.10)
            } else {
                Hsla::from(theme.colors.surface)
            })
            .when(!selected, |card| {
                card.hover(|style| style.bg(Hsla::from(theme.colors.accent).opacity(0.06)))
            })
            .cursor_pointer()
            .child(div().w(px(3.0)).h(px(32.0)).rounded(px(2.0)).bg(stripe))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(2.0))
                                    .text_size(px(theme.font.xs))
                                    .text_color(status_color)
                                    .child(row.status.icon())
                                    .child(row.status.label()),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(theme.font.sm))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.colors.text)
                                    .child(row.task_name.clone()),
                            )
                            .child(div().flex_1())
                            .when_some(row.agent.clone(), |line, agent| {
                                line.child(meta_chip(theme, format!("agent {agent}")))
                            })
                            .when_some(row.attempt, |line, attempt| {
                                line.child(meta_chip(theme, format!("attempt {attempt}")))
                            })
                            .child(
                                div()
                                    .text_size(px(theme.font.xs))
                                    .text_color(theme.colors.muted)
                                    .child(elapsed),
                            ),
                    )
                    .when_some(row.detail.clone(), |body, detail| {
                        body.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(theme.font.xs))
                                .text_color(if row.detail_is_permission {
                                    theme.colors.warning
                                } else {
                                    theme.colors.muted
                                })
                                .child(detail),
                        )
                    }),
            )
            .on_click(cx.listener(move |this, _event, _window, cx| {
                let key = click_key.clone();
                this.state.update(cx, |state, cx| {
                    state.select_inbox_row(key);
                    state.open_inbox_selected();
                    cx.notify();
                });
            }))
    }

    fn recent_section(
        &self,
        theme: &Theme,
        rows: &[RecentRow],
        selected: Option<&InboxKey>,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut section = div().flex().flex_col().gap(px(theme.spacing.s2)).child(
            div()
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .child(components::section_label(theme, "RECENT"))
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.muted)
                        .child("newest first"),
                ),
        );

        if rows.is_empty() {
            section = section.child(
                div()
                    .px(px(theme.spacing.s2))
                    .py(px(theme.spacing.s3))
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.muted)
                    .child("Finished and running tasks will appear here."),
            );
        } else {
            for (index, row) in rows.iter().enumerate() {
                section = section.child(self.recent_row(
                    theme,
                    index,
                    row,
                    selected.is_some_and(|selected| *selected == InboxKey::Task(row.task_id)),
                    cx,
                ));
            }
        }
        section
    }

    fn recent_row(
        &self,
        theme: &Theme,
        index: usize,
        row: &RecentRow,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let task_id = row.task_id;
        let color = status_color(theme, row.status);
        let cost = inbox::format_cost(row.cost.as_ref());

        div()
            .id(("inbox-recent", index))
            .flex()
            .items_center()
            .gap(px(theme.spacing.s3))
            .h(px(32.0))
            .px(px(theme.spacing.s2))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(if selected {
                theme.colors.accent
            } else {
                theme.colors.bg
            })
            .when(selected, |line| {
                line.bg(Hsla::from(theme.colors.accent).opacity(0.10))
            })
            .cursor_pointer()
            .when(!selected, |line| {
                line.hover(|style| style.bg(Hsla::from(theme.colors.accent).opacity(0.06)))
            })
            .child(
                div()
                    .w(px(98.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .text_size(px(theme.font.xs))
                    .text_color(color)
                    .child(components::animated_icon(
                        row.status.icon(),
                        row.status == StatusKind::Running,
                        color,
                        theme.font.xs,
                        components::motion_id("inbox-spinner", task_id),
                    ))
                    .child(row.status.label()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(row.name.clone()),
            )
            .when_some(row.repo.clone(), |line, repo| {
                line.child(
                    div()
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.muted)
                        .child(format!("repo:{repo}")),
                )
            })
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(inbox::format_duration(row.duration)),
            )
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(cost.unwrap_or_else(|| "—".to_string())),
            )
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.select_inbox_row(InboxKey::Task(task_id));
                    state.open_task(task_id);
                    cx.notify();
                });
            }))
    }
}

impl Render for InboxView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.state.read(cx).theme(window.appearance());
        let (rows, status, has_data) = {
            let state = self.state.read(cx);
            (
                state.inbox_rows(),
                state.snapshot_status().clone(),
                state.has_snapshot_data(),
            )
        };
        let selected = self.state.read(cx).selected_inbox_key(&rows);

        if matches!(&status, SnapshotStatus::Loading) && !has_data {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(theme.colors.bg)
                .child(self.header(theme))
                .child(self.loading_body(theme));
        }

        if let SnapshotStatus::Failed { message } = &status {
            if !has_data {
                return div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .bg(theme.colors.bg)
                    .child(self.header(theme))
                    .child(self.error_body(theme, message, cx));
            }
        }

        let body = if rows.is_empty() {
            div()
                .flex_1()
                .min_h_0()
                .child(components::empty_state(
                    theme,
                    Route::Inbox.icon(),
                    "NEEDS YOU",
                    "Nothing needs you right now",
                    "Nothing is blocked or failing. Start a task from the Catalog (g c): choose a definition, fill its declared variables and launch it. Runs that ask for permission or fail land here.",
                    "Start a task from the Catalog",
                ))
                .into_any_element()
        } else {
            let mut content = div()
                .flex()
                .flex_col()
                .gap(px(theme.spacing.s4))
                .p(px(theme.spacing.s4));
            if let SnapshotStatus::Failed { message } = &status {
                content = content.child(self.error_banner(theme, message, cx));
            }
            content
                .child(self.needs_section(theme, &rows.needs, selected.as_ref(), cx))
                .child(self.recent_section(theme, &rows.recent, selected.as_ref(), cx))
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.colors.bg)
            .child(self.header(theme))
            .child(
                div()
                    .id("inbox-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(body),
            )
    }
}

/// A static first-fetch placeholder row. No motion: the skeleton must not fight
/// the OS "reduce motion" setting (docs §3, accessibility).
fn skeleton_row(theme: &Theme, width: f32) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(theme.spacing.s3))
        .h(px(44.0))
        .px(px(theme.spacing.s3))
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(theme.colors.border)
        .bg(theme.colors.surface)
        .child(
            div()
                .w(px(3.0))
                .h(px(28.0))
                .rounded(px(2.0))
                .bg(theme.colors.border),
        )
        .child(
            div()
                .w(px(90.0))
                .h(px(10.0))
                .rounded(px(theme.radii.r4))
                .bg(theme.colors.border),
        )
        .child(
            div()
                .w(px(width))
                .h(px(10.0))
                .rounded(px(theme.radii.r4))
                .bg(theme.colors.border),
        )
}

/// The calm "nothing blocked" line shown above Recent.
fn all_clear(theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(theme.spacing.s2))
        .px(px(theme.spacing.s2))
        .py(px(theme.spacing.s3))
        .child(
            div()
                .text_color(theme.colors.success)
                .text_size(px(theme.font.sm))
                .child("✓"),
        )
        .child(
            div()
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.text)
                .child("Nothing is waiting on you."),
        )
        .child(
            div()
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .child("New permission requests appear here without a refresh."),
        )
}

fn count_pill(theme: &Theme, count: usize, color: Rgba) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .min_w(px(20.0))
        .h(px(18.0))
        .px(px(theme.spacing.s1))
        .rounded(px(theme.radii.r12))
        .border_1()
        .border_color(color)
        .text_color(color)
        .text_size(px(theme.font.xs))
        .child(count.to_string())
}

fn meta_chip(theme: &Theme, label: String) -> Div {
    div()
        .px(px(theme.spacing.s1 + 2.0))
        .py(px(1.0))
        .rounded(px(theme.radii.r4))
        .bg(theme.colors.bg)
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child(label)
}

/// The semantic color for a status. The label + icon always render beside it.
fn status_color(theme: &Theme, status: StatusKind) -> Rgba {
    match status {
        StatusKind::AwaitingInput => theme.colors.warning,
        StatusKind::Failed => theme.colors.danger,
        StatusKind::Running => theme.colors.accent,
        StatusKind::Succeeded => theme.colors.success,
        StatusKind::Pending | StatusKind::Cancelled | StatusKind::Neutral => theme.colors.muted,
    }
}
