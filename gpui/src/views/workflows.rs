//! Workflows: the roots seen in the task snapshot, one root's runtime
//! instances in five buckets, and the catalog DAG source.
//!
//! The aggregation, bucket derivation, progress and optimistic action state
//! live in [`crate::workflow`] (pure and unit-tested); this file only paints
//! them. The roots list and the instance list keep their scroll handles across
//! refreshes, and a live `task.updated` never moves the highlighted instance
//! (docs §2, Working Memory).
//!
//! There is no embedded Graphviz: `workflow.get` is rendered as readable DOT in
//! a monospace panel with a short edge legend (docs §5 step 08).

use gpui::{
    div, prelude::*, px, AnyElement, Context, Div, Entity, FocusHandle, Hsla, IntoElement, Render,
    ScrollHandle, Stateful, Window,
};

use crate::components;
use crate::inbox::StatusKind;
use crate::launch::{self, LaunchFailure};
use crate::routes::Route;
use crate::state::{ClientState, SnapshotStatus};
use crate::theme::Theme;
use crate::workflow::{self, WorkflowBucket, WorkflowDetail, WorkflowGraphState, WorkflowRow};

/// The Workflows surface.
pub struct WorkflowsView {
    state: Entity<ClientState>,
    roots_scroll: ScrollHandle,
    rows_scroll: ScrollHandle,
    dot_scroll: ScrollHandle,
    root_scrolled_to: Option<uuid::Uuid>,
    refresh_focus: FocusHandle,
    cancel_focus: FocusHandle,
    copy_focus: FocusHandle,
    dot_toggle_focus: FocusHandle,
    retry_focus: FocusHandle,
    graph_retry_focus: FocusHandle,
    error_copy_focus: FocusHandle,
    /// The DOT panel is open by default; collapsing it gives the instance
    /// buckets more height on short windows.
    show_dot: bool,
}

impl WorkflowsView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self {
            state,
            roots_scroll: ScrollHandle::new(),
            rows_scroll: ScrollHandle::new(),
            dot_scroll: ScrollHandle::new(),
            root_scrolled_to: None,
            refresh_focus: cx.focus_handle().tab_index(20).tab_stop(true),
            cancel_focus: cx.focus_handle().tab_index(21).tab_stop(true),
            retry_focus: cx.focus_handle().tab_index(22).tab_stop(true),
            error_copy_focus: cx.focus_handle().tab_index(23).tab_stop(true),
            copy_focus: cx.focus_handle().tab_index(24).tab_stop(true),
            dot_toggle_focus: cx.focus_handle().tab_index(25).tab_stop(true),
            graph_retry_focus: cx.focus_handle().tab_index(26).tab_stop(true),
            show_dot: true,
        }
    }

    fn header(&self, theme: &Theme, roots: usize, cx: &mut Context<Self>) -> Div {
        let refresh = components::with_focus(
            div()
                .id("workflows-refresh")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s1))
                .h(px(theme.controls.min))
                .px(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(theme.colors.border)
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.refresh_focus)
                .hover(|style| style.bg(theme.colors.surface))
                .child("↻")
                .child("Refresh")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.refresh_workflow();
                        cx.notify();
                    });
                })),
            theme,
        );

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(theme.spacing.s3))
            .px(px(theme.spacing.s4))
            .py(px(theme.spacing.s3))
            .border_b_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.bg)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(components::section_label(theme, "WORKFLOWS"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .child(
                                div()
                                    .text_size(px(theme.font.lg))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.colors.text)
                                    .child("DAG and progress"),
                            )
                            .child(hint(
                                theme,
                                &format!("{roots} root{}", if roots == 1 { "" } else { "s" }),
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(components::keycap(theme, "↑↓"))
                    .child(hint(theme, "instance"))
                    .child(components::keycap(theme, "↵"))
                    .child(hint(theme, "open"))
                    .child(components::keycap(theme, "[ ]"))
                    .child(hint(theme, "root"))
                    .child(components::keycap(theme, "c"))
                    .child(hint(theme, "cancel root"))
                    .child(components::keycap(theme, "r"))
                    .child(hint(theme, "retry"))
                    .child(refresh),
            )
    }

    /// The roots list is the Workflows surface's own index: one row per root in
    /// the shared `tasks.list` snapshot, newest first.
    fn roots_panel(
        &self,
        theme: &Theme,
        roots: &[workflow::WorkflowRoot],
        selected: Option<uuid::Uuid>,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut list = div()
            .id("workflow-roots")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.roots_scroll)
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(theme.spacing.s2));

        if roots.is_empty() {
            list = list.child(
                div()
                    .p(px(theme.spacing.s2))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child("No roots in the current task list."),
            );
        } else {
            for (index, root) in roots.iter().enumerate() {
                let is_selected = selected == Some(root.root_id);
                let root_id = root.root_id;
                let state_kind = workflow::state_kind(root.state);
                let state_color = status_color(theme, state_kind);
                let row = div()
                    .id(("workflow-root", index))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .w_full()
                    .p(px(theme.spacing.s2))
                    .rounded(px(theme.radii.r4))
                    .border_1()
                    .border_color(if is_selected {
                        Hsla::from(theme.colors.accent)
                    } else {
                        Hsla::from(theme.colors.bg)
                    })
                    .bg(if is_selected {
                        Hsla::from(theme.colors.accent).opacity(0.12)
                    } else {
                        Hsla::from(theme.colors.bg)
                    })
                    .when(!is_selected, |row| {
                        row.hover(|style| style.bg(Hsla::from(theme.colors.accent).opacity(0.05)))
                    })
                    .cursor_pointer()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s1))
                            .min_w_0()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(theme.font.sm))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.colors.text)
                                    .child(root.name.clone()),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(2.0))
                                    .text_size(px(theme.font.xs))
                                    .text_color(state_color)
                                    .child(state_kind.icon())
                                    .child(state_kind.label()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .child(progress_bar(
                                theme,
                                root.progress(),
                                root.total > 0 && root.done >= root.total,
                                64.0,
                            ))
                            .child(
                                div()
                                    .text_size(px(theme.font.xs))
                                    .text_color(theme.colors.muted)
                                    .child(format!("{} / {} done", root.done, root.total)),
                            )
                            .child(div().flex_1())
                            .when(root.active > 0, |line| {
                                line.child(
                                    div()
                                        .text_size(px(theme.font.xs))
                                        .text_color(theme.colors.accent)
                                        .child(format!("{} active", root.active)),
                                )
                            })
                            .when(root.failed > 0, |line| {
                                line.child(
                                    div()
                                        .text_size(px(theme.font.xs))
                                        .text_color(theme.colors.danger)
                                        .child(format!("{} failed", root.failed)),
                                )
                            })
                            .when(root.launch_failures > 0, |line| {
                                line.child(
                                    div()
                                        .text_size(px(theme.font.xs))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme.colors.danger)
                                        .child(format!("⚠ {} spawn failed", root.launch_failures)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child(format!(
                                "{} instance{}",
                                root.total,
                                if root.total == 1 { "" } else { "s" }
                            )),
                    )
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.state.update(cx, |state, cx| {
                            state.select_workflow_root(root_id);
                            cx.notify();
                        });
                    }));
                list = list.child(row);
            }
        }

        div()
            .flex()
            .flex_col()
            .w(px(250.0))
            .flex_shrink_0()
            .border_r_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(theme.spacing.s2))
                    .px(px(theme.spacing.s3))
                    .py(px(theme.spacing.s2))
                    .border_b_1()
                    .border_color(theme.colors.border)
                    .child(components::section_label(theme, "ROOTS"))
                    .child(hint(theme, &roots.len().to_string())),
            )
            .child(list)
    }

    fn roots_loading(&self, theme: &Theme) -> Div {
        let mut list = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s2));
        for index in 0..5 {
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(theme.spacing.s1))
                    .p(px(theme.spacing.s2))
                    .rounded(px(theme.radii.r4))
                    .border_1()
                    .border_color(theme.colors.border)
                    .bg(theme.colors.bg)
                    .child(
                        div()
                            .w(px(120.0 + index as f32 * 16.0))
                            .h(px(10.0))
                            .rounded(px(theme.radii.r4))
                            .bg(theme.colors.border),
                    )
                    .child(
                        div()
                            .w(px(64.0))
                            .h(px(6.0))
                            .rounded(px(theme.radii.r4))
                            .bg(theme.colors.border),
                    ),
            );
        }
        div()
            .flex()
            .flex_col()
            .w(px(250.0))
            .flex_shrink_0()
            .border_r_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(theme.spacing.s2))
                    .px(px(theme.spacing.s3))
                    .py(px(theme.spacing.s2))
                    .border_b_1()
                    .border_color(theme.colors.border)
                    .child(components::section_label(theme, "ROOTS"))
                    .child(hint(theme, "Loading…")),
            )
            .child(list)
    }

    fn roots_error(&self, theme: &Theme, message: &str, cx: &mut Context<Self>) -> Div {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s6))
            .child(
                div()
                    .text_size(px(theme.font.lg))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text)
                    .child("The workflow list could not load"),
            )
            .child(
                div()
                    .max_w(px(520.0))
                    .text_center()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.muted)
                    .child(message.to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(self.error_copy_button(theme, message, "workflow-roots-error-copy", cx))
                    .child(self.retry_button(
                        theme,
                        "workflow-retry-roots-error",
                        "Retry",
                        &self.retry_focus,
                        cx,
                    )),
            )
    }

    fn detail_header(&self, theme: &Theme, detail: &WorkflowDetail, cx: &mut Context<Self>) -> Div {
        let kind = workflow::state_kind(detail.state);
        let color = status_color(theme, kind);
        let cancellable = detail.cancellable();
        let cancelling = detail.pending_cancel;

        let cancel = components::with_focus(
            div()
                .id("workflow-cancel-root")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .h(px(32.0))
                .px(px(theme.spacing.s3))
                .rounded(px(theme.radii.r8))
                .border_1()
                .border_color(if cancellable > 0 {
                    Hsla::from(theme.colors.danger).opacity(0.45)
                } else {
                    Hsla::from(theme.colors.border)
                })
                .bg(if cancellable > 0 {
                    Hsla::from(theme.colors.danger).opacity(0.12)
                } else {
                    Hsla::from(theme.colors.bg)
                })
                .text_size(px(theme.font.sm))
                .text_color(if cancellable > 0 {
                    theme.colors.danger
                } else {
                    theme.colors.muted
                })
                .cursor_pointer()
                .track_focus(&self.cancel_focus)
                .child(if cancelling {
                    "Cancelling…".to_string()
                } else {
                    "Cancel root".to_string()
                })
                .when(cancellable > 0 && !cancelling, |button| {
                    button.child(
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.danger)
                            .child(cancellable.to_string()),
                    )
                })
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.request_workflow_cancel(cx);
                        cx.notify();
                    });
                })),
            theme,
        );

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(theme.spacing.s3))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s3))
            .border_b_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(theme.spacing.s2))
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .min_w_0()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(theme.font.lg))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.colors.text)
                                    .child(detail.root_name.clone()),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(theme.spacing.s1))
                                    .px(px(theme.spacing.s2))
                                    .py(px(3.0))
                                    .rounded(px(theme.radii.r12))
                                    .border_1()
                                    .border_color(Hsla::from(color).opacity(0.45))
                                    .bg(Hsla::from(color).opacity(0.12))
                                    .text_size(px(theme.font.xs))
                                    .text_color(color)
                                    .child(kind.icon())
                                    .child(kind.label()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .child(progress_bar(
                                theme,
                                detail.progress(),
                                detail.total() > 0 && detail.done() >= detail.total(),
                                180.0,
                            ))
                            .child(
                                div()
                                    .text_size(px(theme.font.xs))
                                    .text_color(theme.colors.text)
                                    .child(format!("{} / {} done", detail.done(), detail.total())),
                            )
                            .child(
                                div()
                                    .text_size(px(theme.font.xs))
                                    .text_color(theme.colors.muted)
                                    .child(format!(
                                        "{} instance{} · {} terminal",
                                        detail.total(),
                                        if detail.total() == 1 { "" } else { "s" },
                                        detail.terminal()
                                    )),
                            ),
                    ),
            )
            .child(cancel)
    }

    fn direct_start_banner(&self, theme: &Theme) -> Div {
        div()
            .flex()
            .items_start()
            .gap(px(theme.spacing.s2))
            .mx(px(theme.spacing.s3))
            .mt(px(theme.spacing.s2))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s2))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(Hsla::from(theme.colors.accent).opacity(0.35))
            .bg(Hsla::from(theme.colors.accent).opacity(0.08))
            .child(
                div()
                    .text_color(theme.colors.accent)
                    .text_size(px(theme.font.xs))
                    .child("⋔"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.text)
                    .child(
                        "This run was started directly, so it is its own workflow root. \
                         Children it spawns or unblocks appear here as they are created.",
                    ),
            )
    }

    /// The persistent spawn-failure warning for the selected root. It is derived
    /// from the task rows, so a refresh or reconnect cannot lose it.
    fn launch_banner(&self, theme: &Theme, failures: &[LaunchFailure]) -> Div {
        let lines = launch::banner_lines(failures);
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .mx(px(theme.spacing.s3))
            .mt(px(theme.spacing.s2))
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
                    ),
            )
            .children(lines.into_iter().map(|line| {
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.text)
                    .child(line)
            }))
    }

    fn error_banner(&self, theme: &Theme, message: &str, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s3))
            .mx(px(theme.spacing.s3))
            .mt(px(theme.spacing.s2))
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
                    .child(format!("The workflow could not refresh: {message}")),
            )
            .child(self.error_copy_button(theme, message, "workflow-error-copy", cx))
            .child(self.retry_button(
                theme,
                "workflow-retry-inspect",
                "Retry",
                &self.retry_focus,
                cx,
            ))
    }

    fn retry_button(
        &self,
        theme: &Theme,
        id: &'static str,
        label: &str,
        focus: &FocusHandle,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        components::with_focus(
            div()
                .id(id)
                .flex()
                .items_center()
                .h(px(theme.controls.min))
                .px(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .bg(Hsla::from(theme.colors.accent).opacity(0.16))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.accent)
                .cursor_pointer()
                .track_focus(focus)
                .child(label.to_string())
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.refresh_workflow();
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

    fn loading_body(&self, theme: &Theme, detail: &WorkflowDetail) -> Div {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s3))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(components::section_label(theme, "INSTANCES"))
                    .child(hint(theme, "Loading…")),
            )
            .children((0..6).map(|index| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s3))
                    .h(px(32.0))
                    .px(px(theme.spacing.s3))
                    .rounded(px(theme.radii.r4))
                    .border_1()
                    .border_color(theme.colors.border)
                    .bg(theme.colors.surface)
                    .child(
                        div()
                            .w(px(96.0))
                            .h(px(9.0))
                            .rounded(px(theme.radii.r4))
                            .bg(theme.colors.border),
                    )
                    .child(
                        div()
                            .w(px(180.0 + index as f32 * 30.0))
                            .h(px(9.0))
                            .rounded(px(theme.radii.r4))
                            .bg(theme.colors.border),
                    )
            }))
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(format!(
                        "Reading workflow.inspect for '{}'…",
                        detail.root_name
                    )),
            )
    }

    fn instances_body(
        &self,
        theme: &Theme,
        detail: &WorkflowDetail,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selected = detail.selected_id();
        let mut scroll = div()
            .id("workflow-instances")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.rows_scroll)
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s3));

        let mut row_index = 0usize;
        for section in detail.sections() {
            let bucket = section.bucket;
            let color = bucket_color(theme, bucket);
            let mut block = div().flex().flex_col().gap(px(2.0)).child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .px(px(theme.spacing.s1))
                    .py(px(2.0))
                    .child(
                        div()
                            .text_size(px(theme.font.xs))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(bucket.label()),
                    )
                    .child(
                        div()
                            .px(px(theme.spacing.s1 + 2.0))
                            .rounded(px(theme.radii.r12))
                            .border_1()
                            .border_color(Hsla::from(color).opacity(0.45))
                            .text_size(px(theme.font.xs))
                            .text_color(color)
                            .child(section.rows.len().to_string()),
                    )
                    .child(div().flex_1())
                    .when(section.rows.is_empty(), |header| {
                        header.child(
                            div()
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.muted)
                                .child(bucket.empty()),
                        )
                    }),
            );

            for row in &section.rows {
                let is_selected = selected == Some(row.id);
                block = block.child(self.instance_row(
                    theme,
                    row_index,
                    detail.root_id,
                    row,
                    is_selected,
                    cx,
                ));
                row_index += 1;
            }
            scroll = scroll.child(block);
        }

        scroll
    }

    fn instance_row(
        &self,
        theme: &Theme,
        index: usize,
        root_id: uuid::Uuid,
        row: &WorkflowRow,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let kind = row.status_kind();
        let color = status_color(theme, kind);
        let failed = row.status == favetto_wire::model::TaskStatus::Failed;
        let task_id = row.id;
        let dependency = if !row.dependencies.is_empty() {
            format!("after {}", row.dependencies.join(", "))
        } else if task_id == root_id {
            "root".to_string()
        } else {
            "no predecessor".to_string()
        };
        let retry = (failed || row.retry_pending).then(|| {
            let pending = row.retry_pending;
            components::with_focus(
                div()
                    .id(("workflow-row-retry", index))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .h(px(theme.controls.min))
                    .px(px(theme.spacing.s2))
                    .rounded(px(theme.radii.r4))
                    .border_1()
                    .border_color(Hsla::from(theme.colors.accent).opacity(0.45))
                    .text_size(px(theme.font.xs))
                    .text_color(if pending {
                        theme.colors.muted
                    } else {
                        theme.colors.accent
                    })
                    .cursor_pointer()
                    .child(if pending {
                        "Retrying…".to_string()
                    } else {
                        "Retry".to_string()
                    })
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.state.update(cx, |state, cx| {
                            state.retry_workflow_instance(task_id, cx);
                            cx.notify();
                        });
                    })),
                theme,
            )
        });

        div()
            .id(("workflow-row", index))
            .flex()
            .items_center()
            .gap(px(theme.spacing.s3))
            .w_full()
            .min_h(px(36.0))
            .px(px(theme.spacing.s2))
            .py(px(theme.spacing.s1))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(if selected {
                Hsla::from(theme.colors.accent)
            } else {
                Hsla::from(theme.colors.bg)
            })
            .bg(if selected {
                Hsla::from(theme.colors.accent).opacity(0.10)
            } else {
                Hsla::from(theme.colors.bg)
            })
            .when(!selected, |line| {
                line.hover(|style| style.bg(Hsla::from(theme.colors.accent).opacity(0.05)))
            })
            .cursor_pointer()
            .child(
                div()
                    .w(px(104.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .text_size(px(theme.font.xs))
                    .text_color(color)
                    .child(kind.icon())
                    .child(kind.label()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(theme.font.sm))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.colors.text)
                            .child(row.name.clone()),
                    )
                    .when_some(row.summary.clone(), |cell, summary| {
                        cell.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.danger)
                                .child(summary),
                        )
                    }),
            )
            .child(meta_chip(theme, format!("attempt {}", row.attempt)))
            .child(meta_chip(theme, dependency))
            .when_some(retry, |line, retry| line.child(retry))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.select_workflow_instance(task_id);
                    cx.notify();
                });
            }))
    }

    fn dot_panel(&self, theme: &Theme, graph: &WorkflowGraphState, cx: &mut Context<Self>) -> Div {
        let copy = components::with_focus(
            div()
                .id("workflow-copy-dot")
                .flex()
                .items_center()
                .h(px(theme.controls.min))
                .px(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(theme.colors.border)
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.copy_focus)
                .child("Copy DOT")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.copy_workflow_dot(cx);
                        cx.notify();
                    });
                })),
            theme,
        );

        let toggle = components::with_focus(
            div()
                .id("workflow-toggle-dot")
                .flex()
                .items_center()
                .h(px(theme.controls.min))
                .px(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .cursor_pointer()
                .track_focus(&self.dot_toggle_focus)
                .hover(|style| style.bg(theme.colors.bg))
                .child(if self.show_dot { "Hide" } else { "Show" })
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.show_dot = !this.show_dot;
                    cx.notify();
                })),
            theme,
        );

        let header = div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s2))
            .border_b_1()
            .border_color(theme.colors.border)
            .child(components::section_label(theme, "CATALOG DAG SOURCE"))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family("monospace")
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(graph.path_label().to_string()),
            )
            .child(div().flex_1())
            .when(self.show_dot && !graph.dot.is_empty(), |header| {
                header.child(copy)
            })
            .child(toggle);

        let mut panel = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .max_h(px(if self.show_dot { 240.0 } else { 44.0 }))
            .border_t_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.bg)
            .child(header);

        if !self.show_dot {
            return panel;
        }

        panel = panel.child(
            div()
                .px(px(theme.spacing.s3))
                .py(px(2.0))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .child(WorkflowGraphState::LEGEND),
        );

        let body: AnyElement = if graph.dot.is_empty() {
            if let Some(error) = &graph.error {
                self.graph_error(theme, error, cx).into_any_element()
            } else if graph.loading {
                hint(theme, "Reading workflow.get…").into_any_element()
            } else {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(theme.font.sm))
                            .text_color(theme.colors.text)
                            .child("The catalog has no task definitions yet."),
                    )
                    .child(hint(
                        theme,
                        "The DOT source appears once the daemon discovers definitions.",
                    ))
                    .into_any_element()
            }
        } else {
            div()
                .id("workflow-dot")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.dot_scroll)
                .p(px(theme.spacing.s3))
                .font_family("monospace")
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.text)
                .whitespace_normal()
                .child(graph.dot.clone())
                .into_any_element()
        };

        panel.child(div().flex_1().min_h_0().p(px(theme.spacing.s2)).child(body))
    }

    fn graph_error(&self, theme: &Theme, message: &str, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
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
                    .child(format!("workflow.get failed: {message}")),
            )
            .child(self.error_copy_button(theme, message, "workflow-graph-error-copy", cx))
            .child(self.retry_button(
                theme,
                "workflow-retry-graph",
                "Retry",
                &self.graph_retry_focus,
                cx,
            ))
    }

    fn empty_detail(&self, theme: &Theme) -> Div {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(theme.colors.bg)
            .child(components::empty_state(
                theme,
                Route::Workflows.icon(),
                "WORKFLOWS",
                "No workflows yet",
                "The Workflows list shows the roots in the current task list. Start a task from the Catalog, or spawn one from a run, and its DAG appears here.",
                "Open a run from Tasks (g t)",
            ))
    }

    fn missing_detail(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(theme.colors.bg)
            .child(components::empty_state(
                theme,
                Route::Workflows.icon(),
                "WORKFLOW",
                "Select a root",
                "Choose a root on the left to see its ready, running, failed, blocked and terminal instances. Progress is counted from the instances the daemon returns, never estimated.",
                "Select a root",
            ))
            .child(
                div()
                    .flex()
                    .justify_center()
                    .pb(px(theme.spacing.s4))
                    .child(self.retry_button(
                        theme,
                        "workflow-retry-roots",
                        "Refresh roots",
                        &self.retry_focus,
                        cx,
                    )),
            )
    }
}

impl Render for WorkflowsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.state.read(cx).theme(window.appearance());
        let (roots, selected, detail, graph, snapshot_status, has_snapshot, launch_failures) = {
            let state = self.state.read(cx);
            (
                state.workflow_roots(),
                state.selected_workflow_root(),
                state.workflow_detail().cloned(),
                state.workflow_graph().clone(),
                state.snapshot_status().clone(),
                state.has_snapshot_data(),
                state.launch_failures(),
            )
        };

        if selected != self.root_scrolled_to {
            if let Some(index) =
                selected.and_then(|id| roots.iter().position(|root| root.root_id == id))
            {
                self.roots_scroll.scroll_to_item(index);
            }
            self.root_scrolled_to = selected;
        }

        let body: AnyElement = if roots.is_empty() {
            if matches!(snapshot_status, SnapshotStatus::Loading) && !has_snapshot {
                self.roots_loading(theme).into_any_element()
            } else if let SnapshotStatus::Failed { message } = &snapshot_status {
                if !has_snapshot {
                    self.roots_error(theme, message, cx).into_any_element()
                } else {
                    self.empty_detail(theme).into_any_element()
                }
            } else {
                self.empty_detail(theme).into_any_element()
            }
        } else {
            let Some(detail) = detail else {
                return div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .bg(theme.colors.bg)
                    .child(self.header(theme, roots.len(), cx))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .child(self.roots_panel(theme, &roots, selected, cx))
                            .child(self.missing_detail(theme, cx)),
                    );
            };
            let root_launch_failures: Vec<LaunchFailure> = launch_failures
                .iter()
                .filter(|failure| failure.root_id == detail.root_id)
                .cloned()
                .collect();
            let mut column = div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .bg(theme.colors.bg)
                .child(self.detail_header(theme, &detail, cx));
            if detail.direct_start {
                column = column.child(self.direct_start_banner(theme));
            }
            if !root_launch_failures.is_empty() {
                column = column.child(self.launch_banner(theme, &root_launch_failures));
            }
            if let Some(error) = &detail.error {
                column = column.child(self.error_banner(theme, error, cx));
            }
            if detail.loading && detail.rows.is_empty() {
                column = column.child(self.loading_body(theme, &detail));
            } else if detail.rows.is_empty() {
                column = column.child(
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
                                .text_size(px(theme.font.lg))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.colors.text)
                                .child("This root has no instances"),
                        )
                        .child(
                            div()
                                .max_w(px(460.0))
                                .text_center()
                                .text_size(px(theme.font.sm))
                                .text_color(theme.colors.muted)
                                .child(
                                    "The daemon returned an empty inspect payload. It may have been pruned; refresh to retry.",
                                ),
                        )
                        .child(self.retry_button(
                            theme,
                            "workflow-retry-empty",
                            "Retry",
                            &self.retry_focus,
                            cx,
                        )),
                );
            } else {
                column = column.child(self.instances_body(theme, &detail, cx));
            }
            column = column.child(self.dot_panel(theme, &graph, cx));
            column.into_any_element()
        };

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.colors.bg)
            .child(self.header(theme, roots.len(), cx));
        if let SnapshotStatus::Failed { message } = &snapshot_status {
            if has_snapshot {
                root = root.child(self.error_banner(theme, message, cx));
            }
        }
        let roots_pending = roots.is_empty()
            && !has_snapshot
            && matches!(
                snapshot_status,
                SnapshotStatus::Loading | SnapshotStatus::Failed { .. }
            );
        if roots_pending {
            root.child(body)
        } else {
            root.child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.roots_panel(theme, &roots, selected, cx))
                    .child(body),
            )
        }
    }
}

fn hint(theme: &Theme, text: &str) -> Div {
    div()
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child(text.to_string())
}

fn meta_chip(theme: &Theme, label: String) -> Div {
    div()
        .flex_shrink_0()
        .max_w(px(180.0))
        .truncate()
        .px(px(theme.spacing.s1 + 2.0))
        .py(px(1.0))
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(theme.colors.border)
        .bg(theme.colors.surface)
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child(label)
}

/// A token-sized progress bar. The fill is `done / total`; an unknown total is
/// never drawn as full (docs §2, Goal-Gradient).
fn progress_bar(theme: &Theme, fraction: f32, complete: bool, width: f32) -> Div {
    let fraction = fraction.clamp(0.0, 1.0);
    div()
        .w(px(width))
        .h(px(5.0))
        .flex_shrink_0()
        .rounded(px(3.0))
        .bg(theme.colors.border)
        .overflow_hidden()
        .child(
            div()
                .w(px(width * fraction))
                .h_full()
                .rounded(px(3.0))
                .bg(if complete {
                    theme.colors.success
                } else {
                    theme.colors.accent
                }),
        )
}

fn bucket_color(theme: &Theme, bucket: WorkflowBucket) -> gpui::Rgba {
    match bucket {
        WorkflowBucket::Ready => theme.colors.accent,
        WorkflowBucket::Running => theme.colors.warning,
        WorkflowBucket::Failed => theme.colors.danger,
        WorkflowBucket::Blocked => theme.colors.muted,
        WorkflowBucket::Terminal => theme.colors.success,
    }
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
