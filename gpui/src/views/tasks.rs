//! Tasks: all runs, one status filter and a fuzzy search.
//!
//! The aggregation, counts and match scoring live in [`crate::tasks`] (pure and
//! unit-tested); this file only renders them. The scroll handle is owned by the
//! view and persists across refreshes, so a live `task.updated` update never
//! yanks the reader back to the top (docs §2, Working Memory).
//!
//! Keyboard is owned by the shell (`↑`/`↓`, Home/End, PageUp/PageDown, `Enter`,
//! `c`, `r`, `/`, `[`, `]`); rows and segments are clickable for the same
//! actions, so the surface is usable with either hand (docs §1, Fitts's Law).

use gpui::{
    div, prelude::*, px, uniform_list, AnyElement, Context, Div, Entity, FocusHandle, Hsla,
    IntoElement, Render, ScrollStrategy, Stateful, UniformListScrollHandle, Window,
};
use uuid::Uuid;

use crate::components;
use crate::inbox::{self, StatusKind};
use crate::launch::{self, LaunchFailure};
use crate::routes::Route;
use crate::state::{ClientState, SnapshotStatus};
use crate::tasks::{TaskFilter, TaskList, TaskRow};
use crate::theme::Theme;

/// Fixed height of one virtualized task row (36 px target + 2 px separator).
const TASK_ROW_HEIGHT: f32 = 38.0;

/// The Tasks surface.
pub struct TasksView {
    state: Entity<ClientState>,
    /// Persisted across renders so live updates keep the scroll position.
    scroll: UniformListScrollHandle,
    /// The row that was last scrolled into view, so keyboard navigation keeps
    /// the highlight visible without fighting a manual scroll.
    scrolled_to: Option<Uuid>,
    retry_focus: FocusHandle,
    reset_focus: FocusHandle,
    error_copy_focus: FocusHandle,
    search_clear_focus: FocusHandle,
    launch_dismiss_focus: FocusHandle,
}

impl TasksView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self {
            state,
            scroll: UniformListScrollHandle::new(),
            scrolled_to: None,
            retry_focus: cx.focus_handle().tab_index(21).tab_stop(true),
            reset_focus: cx.focus_handle().tab_index(22).tab_stop(true),
            error_copy_focus: cx.focus_handle().tab_index(23).tab_stop(true),
            search_clear_focus: cx.focus_handle().tab_index(20).tab_stop(true),
            launch_dismiss_focus: cx.focus_handle().tab_index(24).tab_stop(true),
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
                    .child(components::section_label(theme, "TASKS"))
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child("Every run"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(components::keycap(theme, "↑↓"))
                    .child(hint(theme, "move"))
                    .child(components::keycap(theme, "↵"))
                    .child(hint(theme, "open"))
                    .child(components::keycap(theme, "c"))
                    .child(hint(theme, "cancel"))
                    .child(components::keycap(theme, "r"))
                    .child(hint(theme, "retry"))
                    .child(components::keycap(theme, "d"))
                    .child(hint(theme, "dismiss")),
            )
    }

    /// The search field is a div with a fake caret, like the palette input: the
    /// shell keeps keyboard focus so list shortcuts stay live, and `/` plus the
    /// `Search` key context route typed characters here.
    fn search_field(
        &self,
        theme: &Theme,
        query: &str,
        active: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let clear = (!query.is_empty()).then(|| {
            let state = self.state.clone();
            components::with_focus(
                div()
                    .id("tasks-search-clear")
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(theme.controls.min))
                    .px(px(theme.spacing.s2))
                    .rounded(px(theme.radii.r4))
                    .text_color(theme.colors.muted)
                    .cursor_pointer()
                    .track_focus(&self.search_clear_focus)
                    .hover(|style| style.bg(theme.colors.bg))
                    .child("Clear")
                    .on_click(cx.listener(move |_this, _event, _window, cx| {
                        state.update(cx, |state, cx| {
                            state.clear_tasks_search();
                            cx.notify();
                        });
                        cx.stop_propagation();
                    })),
                theme,
            )
        });

        div()
            .id("tasks-search")
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .flex_1()
            .min_w_0()
            .h(px(36.0))
            .px(px(theme.spacing.s3))
            .rounded(px(theme.radii.r8))
            .border_1()
            .border_color(if active {
                theme.colors.accent
            } else {
                theme.colors.border
            })
            .bg(theme.colors.surface)
            .cursor_text()
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.activate_tasks_search();
                    cx.notify();
                });
            }))
            .child(
                div()
                    .text_color(if active {
                        theme.colors.accent
                    } else {
                        theme.colors.muted
                    })
                    .text_size(px(theme.font.base))
                    .child("⌕"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(theme.font.sm))
                            .text_color(if query.is_empty() {
                                theme.colors.muted
                            } else {
                                theme.colors.text
                            })
                            .child(if query.is_empty() {
                                "Search task names…".to_string()
                            } else {
                                query.to_string()
                            }),
                    )
                    .when(active, |text| {
                        text.child(div().text_color(theme.colors.accent).child("▌"))
                    }),
            )
            .when_some(clear, |field, clear| field.child(clear))
            .child(components::keycap(
                theme,
                if active || !query.is_empty() {
                    "esc"
                } else {
                    "/"
                },
            ))
    }

    /// The five segments are one bounded control: a single dimension, with a
    /// count per segment, not a filter maze (docs §2, Hick's Law).
    fn filter_control(
        &self,
        theme: &Theme,
        list: &TaskList,
        active: TaskFilter,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut control = div()
            .flex()
            .items_center()
            .flex_shrink_0()
            .h(px(32.0))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(theme.colors.border)
            .overflow_hidden();

        for (index, filter) in TaskFilter::ALL.iter().enumerate() {
            let selected = *filter == active;
            let filter = *filter;
            let count = list.counts[index];
            let state = self.state.clone();
            control = control.child(
                div()
                    .id(("task-filter", index))
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s1))
                    .h_full()
                    .px(px(theme.spacing.s2))
                    .when(index > 0, |segment| {
                        segment.border_l_1().border_color(theme.colors.border)
                    })
                    .text_size(px(theme.font.xs))
                    .text_color(if selected {
                        theme.colors.accent
                    } else {
                        theme.colors.muted
                    })
                    .when(selected, |segment| {
                        segment.bg(Hsla::from(theme.colors.accent).opacity(0.16))
                    })
                    .when(!selected, |segment| {
                        segment.hover(|style| style.bg(theme.colors.surface))
                    })
                    .cursor_pointer()
                    .when(selected, |segment| segment.child("✓"))
                    .child(filter.label())
                    .child(
                        div()
                            .min_w(px(14.0))
                            .text_center()
                            .text_color(if selected {
                                theme.colors.accent
                            } else {
                                theme.colors.muted
                            })
                            .child(count.to_string()),
                    )
                    .on_click(cx.listener(move |_this, _event, _window, cx| {
                        state.update(cx, |state, cx| {
                            state.set_tasks_filter(filter);
                            cx.notify();
                        });
                    })),
            );
        }
        control
    }

    fn controls(
        &self,
        theme: &Theme,
        list: &TaskList,
        query: &str,
        active: bool,
        filter: TaskFilter,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s3))
            .px(px(theme.spacing.s4))
            .py(px(theme.spacing.s2))
            .border_b_1()
            .border_color(theme.colors.border)
            .child(self.search_field(theme, query, active, cx))
            .child(div().flex_shrink_0().child(components::keycap(theme, "[")))
            .child(self.filter_control(theme, list, filter, cx))
            .child(div().flex_shrink_0().child(components::keycap(theme, "]")))
    }

    fn retry_button(&self, theme: &Theme, cx: &mut Context<Self>) -> Stateful<Div> {
        components::with_focus(
            div()
                .id("tasks-retry")
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
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s4))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(components::section_label(theme, "ALL RUNS"))
                    .child(hint(theme, "Loading…")),
            )
            .children((0..7).map(|index| skeleton_row(theme, 120.0 + index as f32 * 26.0)))
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
            .child(components::section_label(theme, "TASKS"))
            .child(
                div()
                    .text_size(px(theme.font.lg))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text)
                    .child("The task list could not load"),
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
                    .child(self.error_copy_button(theme, message, "tasks-error-copy", cx))
                    .child(self.retry_button(theme, cx)),
            )
            .child(
                div()
                    .max_w(px(480.0))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .text_center()
                    .child(
                        "Check that the daemon is running and the token is readable; the Retry button refreshes without leaving the surface.",
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
            .child(self.error_copy_button(theme, message, "tasks-error-copy-banner", cx))
            .child(self.retry_button(theme, cx))
    }

    /// A persistent warning derived from the task rows, not a toast: a succeeded
    /// parent whose spawned work never launched stays visible across refreshes.
    /// The user can acknowledge it with the header's Dismiss control (or `d`);
    /// the dismissal is process-local and keyed by `(task id, attempt)`.
    fn launch_banner(
        &self,
        theme: &Theme,
        failures: &[LaunchFailure],
        cx: &mut Context<Self>,
    ) -> Div {
        let lines = launch::banner_lines(failures);
        let dismiss_failures = failures.to_vec();
        let dismiss = components::with_focus(
            div()
                .id("tasks-launch-dismiss")
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .h(px(theme.controls.primary))
                .px(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.launch_dismiss_focus)
                .child(components::keycap(theme, "d"))
                .child("Dismiss")
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        if state.dismiss_launch_failures(&dismiss_failures) {
                            cx.notify();
                        }
                    });
                })),
            theme,
        );
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s2))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(Hsla::from(theme.colors.danger).opacity(0.45))
            .bg(Hsla::from(theme.colors.danger).opacity(0.10))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(
                        div()
                            .text_color(theme.colors.danger)
                            .text_size(px(theme.font.xs))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("SPAWN FAILED"),
                    )
                    .child(
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child("the parent succeeded, but its spawned work did not start"),
                    )
                    .child(div().flex_1())
                    .child(dismiss),
            )
            .children(lines.into_iter().map(|line| {
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.text)
                    .child(line)
            }))
    }

    fn filtered_empty(
        &self,
        theme: &Theme,
        list: &TaskList,
        query: &str,
        filter: TaskFilter,
        cx: &mut Context<Self>,
    ) -> Div {
        let reason = if query.trim().is_empty() {
            format!("No run is {}.", filter.label().to_lowercase())
        } else {
            format!("No run matches \"{query}\" in this filter.")
        };
        let reset = components::with_focus(
            div()
                .id("tasks-reset")
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
                .track_focus(&self.reset_focus)
                .child("Clear search and filters")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.reset_tasks_filters();
                        cx.notify();
                    });
                })),
            theme,
        );

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
                    .border_color(Hsla::from(theme.colors.accent).opacity(0.35))
                    .bg(Hsla::from(theme.colors.accent).opacity(0.12))
                    .text_color(theme.colors.accent)
                    .text_size(px(theme.font.xl))
                    .child("⌕"),
            )
            .child(components::section_label(theme, "NO MATCHES"))
            .child(
                div()
                    .text_size(px(theme.font.lg))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text)
                    .child(reason),
            )
            .child(
                div()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.muted)
                    .child(format!(
                        "{} run{} in the list; {} match the search.",
                        list.total,
                        if list.total == 1 { "" } else { "s" },
                        list.matched
                    )),
            )
            .child(reset)
            .child(hint(theme, "Esc clears the search"))
    }
}

/// One task row. Shared by the static (tiny) path and the virtualized list, so
/// both paint exactly the same target and status cluster.
fn task_row(
    theme: &Theme,
    index: usize,
    row: &TaskRow,
    selected: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    let awaiting = row.status == favetto_wire::model::TaskStatus::AwaitingInput;
    let failed = row.status == favetto_wire::model::TaskStatus::Failed;
    let status = row.status_kind();
    let color = status_color(theme, status);
    let stripe = if awaiting {
        theme.colors.accent
    } else if failed {
        theme.colors.danger
    } else {
        theme.colors.border
    };

    div()
        .id(("tasks-row", index))
        .flex()
        .items_center()
        .gap(px(theme.spacing.s3))
        .w_full()
        .h(px(TASK_ROW_HEIGHT - 2.0))
        .py(px(theme.spacing.s1))
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(if selected {
            Hsla::from(theme.colors.accent)
        } else if awaiting {
            Hsla::from(theme.colors.accent).opacity(0.45)
        } else {
            Hsla::from(theme.colors.bg)
        })
        .bg(if selected {
            Hsla::from(theme.colors.accent).opacity(0.10)
        } else if awaiting {
            Hsla::from(theme.colors.accent).opacity(0.06)
        } else {
            Hsla::from(theme.colors.bg)
        })
        .when(!selected, |line| {
            line.hover(|style| style.bg(Hsla::from(theme.colors.accent).opacity(0.05)))
        })
        .cursor_pointer()
        // Von Restorff: awaiting rows carry the accent stripe + icon; the
        // label always accompanies the color (docs §2/§3).
        .child(
            div()
                .w(px(3.0))
                .h(px(22.0))
                .flex_shrink_0()
                .rounded(px(2.0))
                .bg(stripe),
        )
        .child(
            div()
                .w(px(108.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(2.0))
                .text_size(px(theme.font.xs))
                .text_color(color)
                .child(status.icon())
                .child(status.label()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(theme.font.sm))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.colors.text)
                .child(row.name.clone()),
        )
        .when_some(row.repo.clone(), |line, repo| {
            line.child(meta_chip(theme, format!("repo:{repo}")))
        })
        .when_some(row.agent.clone(), |line, agent| {
            line.child(meta_chip(theme, format!("agent {agent}")))
        })
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .child(format!("attempt {}", row.attempt)),
        )
        .child(
            div()
                .w(px(58.0))
                .flex_shrink_0()
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .child(inbox::format_duration(row.duration)),
        )
        .when_some(inbox::format_cost(row.cost.as_ref()), |line, cost| {
            line.child(
                div()
                    .flex_shrink_0()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(cost),
            )
        })
        .on_click(on_click)
}

impl Render for TasksView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.state.read(cx).theme(window.appearance());
        let (list, status, query, search_active, filter, launch_failures) = {
            let state = self.state.read(cx);
            (
                state.task_list(),
                state.snapshot_status().clone(),
                state.tasks_query().to_string(),
                state.is_tasks_search_active(),
                state.tasks_filter(),
                state.launch_failures(),
            )
        };
        let selected = self.state.read(cx).selected_tasks_id();

        // Keep keyboard selection visible: Home/End and PageUp/PageDown can
        // move the highlight far past the viewport. Non-strict scrolling means
        // a row that is already visible never shifts the list.
        if selected != self.scrolled_to {
            if let Some(index) = selected.and_then(|id| list.position(id)) {
                self.scroll.scroll_to_item(index, ScrollStrategy::Center);
            }
            self.scrolled_to = selected;
        }

        if matches!(&status, SnapshotStatus::Loading) && list.total == 0 {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(theme.colors.bg)
                .child(self.header(theme))
                .child(self.controls(theme, &list, &query, search_active, filter, cx))
                .child(self.loading_body(theme));
        }

        if let SnapshotStatus::Failed { message } = &status {
            if list.total == 0 {
                return div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .bg(theme.colors.bg)
                    .child(self.header(theme))
                    .child(self.controls(theme, &list, &query, search_active, filter, cx))
                    .child(self.error_body(theme, message, cx));
            }
        }

        let show_rows = !list.rows.is_empty();

        // Long lists are virtualized: only the visible window is painted and
        // measured. The row data itself is an `Rc` from the state cache, so a
        // render never clones the snapshot.
        let scroll: AnyElement = if list.is_truly_empty() {
            div()
                .flex_1()
                .min_h_0()
                .child(components::empty_state(
                    theme,
                    Route::Tasks.icon(),
                    "ALL RUNS",
                    "No runs yet",
                    "Every run the daemon starts appears here with its status, agent, attempt and elapsed time. Start a task from the Catalog (g c) and it lands here, live.",
                    "Start a task from the Catalog (g c)",
                ))
                .into_any_element()
        } else if list.is_filtered_empty() {
            div()
                .flex_1()
                .min_h_0()
                .child(self.filtered_empty(theme, &list, &query, filter, cx))
                .into_any_element()
        } else {
            let rows = list.clone();
            let view = cx.entity();
            let list_theme = *theme;
            let selected_id = selected;
            uniform_list("tasks-list", rows.rows.len(), move |range, _window, _cx| {
                range
                    .filter_map(|index| {
                        let row = rows.rows.get(index)?;
                        let is_selected = selected_id == Some(row.id);
                        let view = view.clone();
                        let task_id = row.id;
                        Some(
                            div()
                                .h(px(TASK_ROW_HEIGHT))
                                .px(px(list_theme.spacing.s2))
                                .py(px(1.0))
                                .child(task_row(
                                    &list_theme,
                                    index,
                                    row,
                                    is_selected,
                                    move |_event, _window, cx| {
                                        view.update(cx, |this, cx| {
                                            this.state.update(cx, |state, cx| {
                                                state.select_tasks_row(task_id);
                                                state.deactivate_tasks_search();
                                                state.open_task(task_id);
                                                cx.notify();
                                            });
                                        });
                                    },
                                ))
                                .into_any_element(),
                        )
                    })
                    .collect()
            })
            .track_scroll(self.scroll.clone())
            .flex_1()
            .min_h_0()
            .into_any_element()
        };

        let mut list_header = div()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s3));
        if let SnapshotStatus::Failed { message } = &status {
            list_header = list_header.child(self.error_banner(theme, message, cx));
        }
        if !launch_failures.is_empty() {
            list_header = list_header.child(self.launch_banner(theme, &launch_failures, cx));
        }
        if show_rows {
            list_header = list_header.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .px(px(theme.spacing.s1))
                    .child(components::section_label(theme, "ALL RUNS"))
                    .child(hint(
                        theme,
                        &format!("{} shown of {} · newest first", list.rows.len(), list.total),
                    ))
                    .child(div().flex_1())
                    .child(hint(theme, "selected row highlights; ↵ opens")),
            );
        }

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.colors.bg)
            .child(self.header(theme))
            .child(self.controls(theme, &list, &query, search_active, filter, cx));
        if show_rows
            || !launch_failures.is_empty()
            || matches!(&status, SnapshotStatus::Failed { .. })
        {
            root = root.child(list_header);
        }
        root.child(scroll)
    }
}

fn hint(theme: &Theme, text: &str) -> Div {
    div()
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child(text.to_string())
}

fn meta_chip(theme: &Theme, label: String) -> Div {
    // Agent and repo strings come from the daemon and are arbitrary text; cap
    // and truncate them so one long value cannot widen the whole row.
    div()
        .flex_shrink()
        .min_w(px(0.0))
        .max_w(px(150.0))
        .truncate()
        .px(px(theme.spacing.s1 + 2.0))
        .py(px(1.0))
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(theme.colors.border)
        .bg(theme.colors.bg)
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child(label)
}

/// A static first-fetch placeholder row. No motion: the skeleton must not fight
/// the OS "reduce motion" setting (docs §3, accessibility).
fn skeleton_row(theme: &Theme, width: f32) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(theme.spacing.s3))
        .h(px(36.0))
        .px(px(theme.spacing.s3))
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(theme.colors.border)
        .bg(theme.colors.surface)
        .child(
            div()
                .w(px(3.0))
                .h(px(22.0))
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

/// The semantic color for a status. The label + icon always render beside it.
fn status_color(theme: &Theme, status: StatusKind) -> gpui::Rgba {
    match status {
        StatusKind::AwaitingInput => theme.colors.warning,
        StatusKind::Failed => theme.colors.danger,
        StatusKind::Running => theme.colors.accent,
        StatusKind::Succeeded => theme.colors.success,
        StatusKind::Pending | StatusKind::Cancelled | StatusKind::Neutral => theme.colors.muted,
    }
}
