//! The task detail surface: a pinned task header, the Output/Runs tabs and the
//! task actions. The structured conversation inside the Output tab is the
//! shared [`crate::views::conversation::ConversationPanel`], also used by the
//! free-standing Agent surface.
//!
//! Working Memory: the header never scrolls away; only the tab body scrolls.
//! Peak-End: a finished run ends with its outcome and usage in the header, and
//! every tab has an explicit empty/error state with Retry. Von Restorff: a
//! blocked run gets the warning-colored permission banner from the shared
//! conversation view; Cancel (live) and Delete (terminal) are mutually
//! exclusive by status, so at most one danger-colored action is visible.

use std::time::Duration;

use favetto_wire::model::{AgentActivity, AgentUsage, Task, TaskRun};
use favetto_wire::Timestamp;
use gpui::{
    div, prelude::*, px, AnyElement, Context, Div, Entity, FocusHandle, Hsla, IntoElement, Render,
    ScrollHandle, Stateful, Window,
};

use crate::components;
use crate::detail::{self, DetailTab};
use crate::inbox::{self, StatusKind};
use crate::layout::{container_query, LayoutMode};
use crate::state::ClientState;
use crate::tasks;
use crate::theme::Theme;
use crate::views::conversation::ConversationPanel;

/// The detail surface.
pub struct DetailView {
    state: Entity<ClientState>,
    conversation: Entity<ConversationPanel>,
    back_focus: FocusHandle,
    tab_focus: Vec<FocusHandle>,
    cancel_focus: FocusHandle,
    interrupt_focus: FocusHandle,
    retry_focus: FocusHandle,
    copy_focus: FocusHandle,
    delete_focus: FocusHandle,
    workflow_focus: FocusHandle,
    runs_retry_focus: FocusHandle,
    error_copy_focus: FocusHandle,
    /// Scroll state for the Runs attempt list.
    runs_scroll: ScrollHandle,
    runs_scrollbar: components::ScrollbarDrag,
}

impl DetailView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        let conversation = cx.new(|cx| ConversationPanel::new(state.clone(), cx));
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        let view = Self {
            state,
            conversation,
            back_focus: cx.focus_handle().tab_index(50).tab_stop(true),
            tab_focus: (0..DetailTab::ALL.len())
                .map(|index| {
                    cx.focus_handle()
                        .tab_index(59 + index as isize)
                        .tab_stop(true)
                })
                .collect(),
            cancel_focus: cx.focus_handle().tab_index(53).tab_stop(true),
            interrupt_focus: cx.focus_handle().tab_index(68).tab_stop(true),
            retry_focus: cx.focus_handle().tab_index(54).tab_stop(true),
            copy_focus: cx.focus_handle().tab_index(55).tab_stop(true),
            delete_focus: cx.focus_handle().tab_index(49).tab_stop(true),
            workflow_focus: cx.focus_handle().tab_index(52).tab_stop(true),
            runs_retry_focus: cx.focus_handle().tab_index(65).tab_stop(true),
            error_copy_focus: cx.focus_handle().tab_index(67).tab_stop(true),
            runs_scroll: ScrollHandle::new(),
            runs_scrollbar: components::ScrollbarDrag::new(),
        };
        view.start_clock(cx);
        view
    }

    /// Keep `elapsed` honest for a live run even when the agent is quiet.
    fn start_clock(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |weak, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let alive = weak
                .update(cx, |this: &mut Self, cx| {
                    let live = this
                        .state
                        .read(cx)
                        .selected_task()
                        .is_some_and(|task| !tasks::is_terminal(task.status));
                    if live {
                        cx.notify();
                    }
                })
                .is_ok();
            if !alive {
                break;
            }
        })
        .detach();
    }

    fn header(
        &self,
        theme: &Theme,
        data: HeaderData<'_>,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let task = data.task;
        let status = StatusKind::from_task_status(task.status);
        let color = status_color(theme, status);
        let name = inbox::task_display_name(&task.name);
        let repo = inbox::task_repo(&task.name);
        let elapsed = inbox::task_duration(task, Timestamp::now())
            .map(|duration| inbox::format_duration(Some(duration)))
            .unwrap_or_else(|| "—".to_string());
        let usage = detail::usage_summary(data.usage);
        let (live_session, cancel_pending) = {
            let state = self.state.read(cx);
            (
                state.detail_live_session_id(),
                state.detail_turn_cancel_pending(),
            )
        };

        let interrupt = live_session.map(|_| {
            self.interrupt_button(
                theme,
                if cancel_pending {
                    "Interrupting…"
                } else {
                    "Interrupt turn"
                },
                !cancel_pending,
                cancel_pending,
                cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.interrupt_detail_turn(cx);
                        cx.notify();
                    });
                }),
            )
        });

        let workflow = self.action_button(
            theme,
            "detail-workflow",
            "View workflow",
            false,
            &self.workflow_focus,
            cx.listener(|this, _event, _window, cx| {
                let task_id = this.state.read(cx).selected_task_id();
                if let Some(task_id) = task_id {
                    this.state.update(cx, |state, cx| {
                        state.open_workflow_for_task(task_id, cx);
                        cx.notify();
                    });
                }
            }),
        );
        let cancel = self.action_button(
            theme,
            "detail-cancel",
            "Cancel",
            false,
            &self.cancel_focus,
            cx.listener(|this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.request_detail_action(tasks::TaskCommand::Cancel, cx);
                    cx.notify();
                });
            }),
        );
        let retry = self.action_button(
            theme,
            "detail-retry",
            "Retry",
            false,
            &self.retry_focus,
            cx.listener(|this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.request_detail_action(tasks::TaskCommand::Retry, cx);
                    cx.notify();
                });
            }),
        );
        let copy = self.action_button(
            theme,
            "detail-copy",
            "Copy output",
            false,
            &self.copy_focus,
            cx.listener(|this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.copy_detail_output(cx);
                    cx.notify();
                });
            }),
        );
        // Delete is terminal-only and sits apart from the non-destructive
        // controls, so a live run can never be removed by mistake.
        let delete = tasks::can_delete(task.status).then(|| {
            self.action_button(
                theme,
                "detail-delete",
                "Delete run",
                true,
                &self.delete_focus,
                cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.request_detail_action(tasks::TaskCommand::Delete, cx);
                        cx.notify();
                    });
                }),
            )
        });

        div()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s2))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s3))
            .border_b_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
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
                            .gap(px(theme.spacing.s1))
                            .flex_shrink_0()
                            .px(px(theme.spacing.s2))
                            .py(px(3.0))
                            .rounded(px(theme.radii.r12))
                            .border_1()
                            .border_color(Hsla::from(color).opacity(0.45))
                            .bg(Hsla::from(color).opacity(0.12))
                            .text_size(px(theme.font.xs))
                            .text_color(color)
                            .child(components::animated_icon(
                                status.icon(),
                                status == StatusKind::Running,
                                color,
                                theme.font.xs,
                                components::motion_id("detail-header-spinner", task.id),
                            ))
                            .child(status.label()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child(name.to_string()),
                    )
                    .when(narrow, |row| row.child(self.back_button(theme, cx))),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(theme.spacing.s4))
                    .gap_y(px(theme.spacing.s1))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .when_some(repo.map(str::to_string), |row, repo| {
                        row.child(meta(theme, "repo", &repo))
                    })
                    .child(meta(theme, "agent", data.agent.unwrap_or("—")))
                    .child(meta(theme, "attempt", &task.attempt.to_string()))
                    .child(meta(theme, "elapsed", &elapsed))
                    .when_some(usage, |row, usage| row.child(meta(theme, "usage", &usage))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .min_w_0()
                    .child(div().flex_1())
                    .when_some(interrupt, |row, interrupt| row.child(interrupt))
                    .child(workflow)
                    .child(cancel)
                    .child(retry)
                    .child(copy)
                    .when_some(delete, |row, delete| {
                        row.child(
                            div()
                                .flex_shrink_0()
                                .w(px(1.0))
                                .h(px(18.0))
                                .bg(theme.colors.border),
                        )
                        .child(delete)
                    }),
            )
    }

    fn back_button(&self, theme: &Theme, cx: &mut Context<Self>) -> Stateful<Div> {
        let state = self.state.clone();
        components::with_focus(
            div()
                .id("detail-back")
                .flex_shrink_0()
                .px(px(theme.spacing.s2))
                .py(px(2.0))
                .rounded(px(theme.radii.r4))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.back_focus)
                .child("← Back")
                .on_click(cx.listener(move |_this, _event, _window, cx| {
                    state.update(cx, |state, cx| {
                        state.close_detail();
                        cx.notify();
                    });
                })),
            theme,
        )
    }

    fn action_button(
        &self,
        theme: &Theme,
        id: &'static str,
        label: &'static str,
        danger: bool,
        focus: &FocusHandle,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> Stateful<Div> {
        let color = if danger {
            theme.colors.danger
        } else {
            theme.colors.text
        };
        components::with_focus(
            div()
                .id(id)
                .flex_shrink_0()
                .flex()
                .items_center()
                .h(px(theme.controls.min))
                .px(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(if danger {
                    Hsla::from(theme.colors.danger).opacity(0.45)
                } else {
                    Hsla::from(theme.colors.border)
                })
                .text_size(px(theme.font.xs))
                .text_color(color)
                .cursor_pointer()
                .hover(|style| style.bg(theme.colors.bg))
                .track_focus(focus)
                .child(label)
                .on_click(on_click),
            theme,
        )
    }

    fn interrupt_button(
        &self,
        theme: &Theme,
        label: &'static str,
        enabled: bool,
        spinning: bool,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> Stateful<Div> {
        components::with_focus(
            div()
                .id("detail-interrupt")
                .flex_shrink_0()
                .flex()
                .items_center()
                .h(px(theme.controls.min))
                .gap(px(theme.spacing.s1))
                .px(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(Hsla::from(theme.colors.border))
                .text_size(px(theme.font.xs))
                .when(spinning, |button| {
                    button.child(components::animated_icon(
                        "◐",
                        true,
                        theme.colors.accent,
                        theme.font.xs,
                        components::motion_id("detail-interrupt-spinner", "turn"),
                    ))
                })
                .when(enabled, |button| {
                    button
                        .text_color(theme.colors.text)
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.colors.bg))
                        .track_focus(&self.interrupt_focus)
                        .on_click(on_click)
                })
                .when(!enabled, |button| {
                    button
                        .text_color(theme.colors.muted)
                        .opacity(0.55)
                        .cursor_default()
                })
                .child(label),
            theme,
        )
    }

    fn tabs(&self, theme: &Theme, tab: DetailTab, cx: &mut Context<Self>) -> Div {
        let mut bar = div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s1))
            .px(px(theme.spacing.s3))
            .h(px(38.0))
            .border_b_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface);

        for (index, candidate) in DetailTab::ALL.iter().enumerate() {
            let selected = *candidate == tab;
            let candidate = *candidate;
            bar = bar.child(components::with_focus(
                div()
                    .id(("detail-tab", index))
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s1))
                    .h(px(theme.controls.min))
                    .px(px(theme.spacing.s2))
                    .rounded(px(theme.radii.r4))
                    .when(selected, |button| {
                        button
                            .bg(Hsla::from(theme.colors.accent).opacity(0.16))
                            .text_color(theme.colors.accent)
                    })
                    .when(!selected, |button| {
                        button
                            .text_color(theme.colors.muted)
                            .hover(|style| style.bg(theme.colors.bg))
                    })
                    .text_size(px(theme.font.sm))
                    .cursor_pointer()
                    .track_focus(&self.tab_focus[index])
                    .child(candidate.label())
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.state.update(cx, |state, cx| {
                            state.set_detail_tab(candidate);
                            cx.notify();
                        });
                    })),
                theme,
            ));
        }
        bar
    }

    fn loading(&self, theme: &Theme, label: &str) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s4))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(components::section_label(theme, label))
                    .child(hint(theme, "Loading…")),
            )
            .children((0..5).map(|index| skeleton_line(theme, 130.0 + index as f32 * 30.0)))
    }

    fn error_body(&self, theme: &Theme, data: ErrorBody<'_>, cx: &mut Context<Self>) -> Div {
        let retry = components::with_focus(
            div()
                .id("detail-tab-retry")
                .flex()
                .items_center()
                .h(px(theme.controls.primary))
                .px(px(theme.spacing.s3))
                .rounded(px(theme.radii.r8))
                .bg(Hsla::from(theme.colors.accent).opacity(0.16))
                .text_color(theme.colors.accent)
                .text_size(px(theme.font.sm))
                .cursor_pointer()
                .track_focus(data.focus)
                .child("Retry")
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.retry_detail_runs();
                        cx.notify();
                    });
                })),
            theme,
        );
        let copy =
            components::copy_button(theme, "detail-error-copy", "Copy error", data.copy_focus);
        let message = data.message.to_string();
        let copy = copy.on_click(cx.listener(move |this, _event, _window, cx| {
            this.state.update(cx, |state, cx| {
                state.copy_text(message.clone(), cx);
                cx.notify();
            });
        }));
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s6))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(42.0))
                    .h(px(42.0))
                    .rounded(px(theme.radii.r12))
                    .border_1()
                    .border_color(Hsla::from(theme.colors.danger).opacity(0.35))
                    .bg(Hsla::from(theme.colors.danger).opacity(0.12))
                    .text_color(theme.colors.danger)
                    .text_size(px(theme.font.lg))
                    .child("⚠"),
            )
            .child(components::section_label(theme, data.label))
            .child(
                div()
                    .text_size(px(theme.font.sm))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text)
                    .text_center()
                    .child(data.title.to_string()),
            )
            .child(
                div()
                    .max_w(px(320.0))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .text_center()
                    .child(data.message.to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(copy)
                    .child(retry),
            )
    }

    fn empty_body(&self, theme: &Theme, icon: &str, label: &str, title: &str, body: &str) -> Div {
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s6))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(44.0))
                    .h(px(44.0))
                    .rounded(px(theme.radii.r12))
                    .border_1()
                    .border_color(Hsla::from(theme.colors.accent).opacity(0.35))
                    .bg(Hsla::from(theme.colors.accent).opacity(0.12))
                    .text_color(theme.colors.accent)
                    .text_size(px(theme.font.lg))
                    .child(icon.to_string()),
            )
            .child(components::section_label(theme, label))
            .child(
                div()
                    .text_size(px(theme.font.sm))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text)
                    .text_center()
                    .child(title.to_string()),
            )
            .child(
                div()
                    .max_w(px(320.0))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .text_center()
                    .child(body.to_string()),
            )
    }

    fn runs_body(
        &mut self,
        theme: &Theme,
        data: RunsBody<'_>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let RunsBody {
            runs,
            current_attempt,
            activity,
            loading,
            loaded,
            error: runs_error,
        } = data;
        if loading && runs.is_empty() && !loaded {
            return self.loading(theme, "RUNS").into_any_element();
        }
        if runs.is_empty() && runs_error.is_some() {
            return self
                .error_body(
                    theme,
                    ErrorBody {
                        label: "RUNS",
                        title: "The attempt table could not load",
                        message: runs_error.as_deref().unwrap_or_default(),
                        focus: &self.runs_retry_focus,
                        copy_focus: &self.error_copy_focus,
                    },
                    cx,
                )
                .into_any_element();
        }
        if runs.is_empty() {
            return self
                .empty_body(
                    theme,
                    "↻",
                    "RUNS",
                    "No attempts recorded",
                    "The daemon records a run row for every execution attempt. A queued task gets its first row when it starts.",
                )
                .into_any_element();
        }

        let now = Timestamp::now();
        let mut column = div()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s2));
        if let Some(message) = runs_error {
            column = column.child(
                div()
                    .px(px(theme.spacing.s3))
                    .py(px(theme.spacing.s2))
                    .rounded(px(theme.radii.r4))
                    .border_1()
                    .border_color(Hsla::from(theme.colors.danger).opacity(0.45))
                    .bg(Hsla::from(theme.colors.danger).opacity(0.10))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.danger)
                    .child(format!("Refresh failed: {message}")),
            );
        }
        column = column.child(runs_header(theme));
        for run in runs.iter().rev() {
            let live = (run.attempt == current_attempt)
                .then_some(activity)
                .flatten();
            column = column.child(run_row(theme, run, current_attempt, now, live));
        }
        let runs_view = cx.entity().downgrade();
        let scrollbar = components::vertical_scrollbar(
            theme,
            "detail-runs",
            components::ScrollbarSource::Handle(self.runs_scroll.clone()),
            &self.runs_scrollbar,
            move |_window, cx| {
                let _ = runs_view.update(cx, |_this, cx| cx.notify());
            },
        );
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id("detail-runs-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.runs_scroll)
                    .child(column),
            )
            .when_some(scrollbar, |pane, bar| pane.child(bar))
            .into_any_element()
    }

    /// The title strip shown before a run is selected.
    fn plain_route_header(&self, theme: &Theme, narrow: bool, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(theme.spacing.s2))
            .h(px(44.0))
            .px(px(theme.spacing.s3))
            .border_b_1()
            .border_color(theme.colors.border)
            .child(components::section_label(theme, "DETAIL"))
            .when(narrow, |header| header.child(self.back_button(theme, cx)))
    }
}

/// The identity cluster the pinned header renders.
struct HeaderData<'a> {
    task: &'a Task,
    agent: Option<&'a str>,
    usage: Option<&'a AgentUsage>,
}

/// The copy for one tab's full-body error state.
struct ErrorBody<'a> {
    label: &'a str,
    title: &'a str,
    message: &'a str,
    focus: &'a FocusHandle,
    copy_focus: &'a FocusHandle,
}

/// Everything the Runs tab body needs, extracted from the state in one go.
struct RunsBody<'a> {
    runs: &'a [TaskRun],
    current_attempt: u32,
    activity: Option<&'a AgentActivity>,
    loading: bool,
    loaded: bool,
    error: Option<String>,
}

impl Render for DetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.state.read(cx).theme(window.appearance());
        let narrow = container_query(window.viewport_size().width) == LayoutMode::Narrow;

        let (task, agent, usage, tab, activity) = {
            let state = self.state.read(cx);
            (
                state.selected_task().cloned(),
                state.detail_agent(),
                state.detail_usage(),
                state.detail_tab(),
                state.detail_agent_activity(),
            )
        };

        let Some(task) = task else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(theme.colors.surface)
                .child(self.plain_route_header(theme, narrow, cx))
                .child(components::empty_state(
                    theme,
                    "◎",
                    "RUN",
                    "No run selected",
                    "Open a run in the list to see its status, agent and attempt here. Its output and attempt history stay one tab away.",
                    "Open a row to inspect it",
                ));
        };

        let header = self.header(
            theme,
            HeaderData {
                task: &task,
                agent: agent.as_deref(),
                usage: usage.as_ref(),
            },
            narrow,
            cx,
        );
        let tabs = self.tabs(theme, tab, cx);
        let current_attempt = task.attempt;

        let body = match tab {
            DetailTab::Output => self.conversation.clone().into_any_element(),
            DetailTab::Runs => {
                let (runs, loading, loaded, error) = {
                    let state = self.state.read(cx);
                    let detail = state.detail();
                    (
                        detail.map(|detail| detail.runs.clone()).unwrap_or_default(),
                        detail.is_some_and(|detail| detail.runs_loading),
                        detail.is_some_and(|detail| detail.runs_loaded),
                        detail.and_then(|detail| detail.runs_error.clone()),
                    )
                };
                self.runs_body(
                    theme,
                    RunsBody {
                        runs: &runs,
                        current_attempt,
                        activity: activity.as_ref(),
                        loading,
                        loaded,
                        error,
                    },
                    cx,
                )
            }
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.colors.surface)
            .child(header)
            .child(tabs)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
    }
}

fn meta(theme: &Theme, label: &str, value: &str) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(theme.spacing.s1))
        .min_w_0()
        .child(
            div()
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .child(label.to_string()),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.text)
                .child(value.to_string()),
        )
}

fn hint(theme: &Theme, text: &str) -> Div {
    div()
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child(text.to_string())
}

fn status_color(theme: &Theme, status: StatusKind) -> gpui::Rgba {
    match status {
        StatusKind::AwaitingInput => theme.colors.warning,
        StatusKind::Failed => theme.colors.danger,
        StatusKind::Running => theme.colors.accent,
        StatusKind::Succeeded => theme.colors.success,
        StatusKind::Pending | StatusKind::Cancelled | StatusKind::Neutral => theme.colors.muted,
    }
}

fn runs_header(theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(theme.spacing.s2))
        .px(px(theme.spacing.s2))
        .py(px(theme.spacing.s1))
        .text_size(px(theme.font.xs))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.colors.muted)
        .child(div().w(px(28.0)).flex_shrink_0().child("#"))
        .child(div().flex_1().min_w_0().child("STATUS"))
        .child(div().w(px(38.0)).flex_shrink_0().child("EXIT"))
        .child(div().w(px(52.0)).flex_shrink_0().child("TIME"))
        .child(
            div()
                .w(px(72.0))
                .flex_shrink_0()
                .text_right()
                .child("USAGE"),
        )
}

fn run_row(
    theme: &Theme,
    run: &TaskRun,
    current_attempt: u32,
    now: Timestamp,
    activity: Option<&AgentActivity>,
) -> Div {
    let status = detail::run_status_kind(run.status);
    let color = status_color(theme, status);
    let current = run.attempt == current_attempt;
    let duration = detail::run_duration(run, now)
        .map(|duration| inbox::format_duration(Some(duration)))
        .unwrap_or_else(|| "—".to_string());
    let usage = detail::usage_summary(run.usage.as_ref()).unwrap_or_else(|| "—".to_string());

    let mut row = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px(px(theme.spacing.s2))
        .py(px(theme.spacing.s1))
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(if current {
            Hsla::from(theme.colors.accent).opacity(0.45)
        } else {
            Hsla::from(theme.colors.bg)
        })
        .when(current, |row| {
            row.bg(Hsla::from(theme.colors.accent).opacity(0.08))
        })
        .when(!current, |row| row.hover(|style| style.bg(theme.colors.bg)))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .text_size(px(theme.font.xs))
                .child(
                    div()
                        .w(px(28.0))
                        .flex_shrink_0()
                        .text_color(theme.colors.text)
                        .child(run.attempt.to_string()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(2.0))
                        .text_color(color)
                        .child(components::animated_icon(
                            status.icon(),
                            status == StatusKind::Running,
                            color,
                            theme.font.xs,
                            components::motion_id("run-spinner", run.id),
                        ))
                        .child(detail::run_status_label(run.status))
                        .when_some(activity.and_then(detail::activity_label), |row, label| {
                            row.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme.colors.muted)
                                    .child(format!("· {label}")),
                            )
                        }),
                )
                .child(
                    div()
                        .w(px(38.0))
                        .flex_shrink_0()
                        .font_family("monospace")
                        .text_color(theme.colors.muted)
                        .child(detail::run_exit_label(run)),
                )
                .child(
                    div()
                        .w(px(52.0))
                        .flex_shrink_0()
                        .text_color(theme.colors.muted)
                        .child(duration),
                )
                .child(
                    div()
                        .w(px(72.0))
                        .flex_shrink_0()
                        .truncate()
                        .text_right()
                        .text_color(theme.colors.muted)
                        .child(usage),
                ),
        );
    if let Some(error) = &run.error {
        row = row.child(
            div()
                .w_full()
                .min_w_0()
                .truncate()
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.danger)
                .child(error.clone()),
        );
    }
    row
}

/// A static first-fetch placeholder. No motion (the OS "reduce motion" setting
/// is respected; docs §3).
fn skeleton_line(theme: &Theme, width: f32) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(theme.spacing.s2))
        .child(
            div()
                .w(px(70.0))
                .h(px(9.0))
                .rounded(px(theme.radii.r4))
                .bg(theme.colors.border),
        )
        .child(
            div()
                .w(px(width))
                .h(px(9.0))
                .rounded(px(theme.radii.r4))
                .bg(theme.colors.border),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_color_distinguishes_blocked_failed_and_running() {
        let theme = Theme::for_appearance(gpui::WindowAppearance::Dark);
        assert_eq!(
            status_color(theme, StatusKind::AwaitingInput),
            theme.colors.warning
        );
        assert_eq!(status_color(theme, StatusKind::Failed), theme.colors.danger);
        assert_ne!(
            status_color(theme, StatusKind::Running),
            status_color(theme, StatusKind::Succeeded)
        );
    }
}
