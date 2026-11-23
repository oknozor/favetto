//! Workflows: the roots seen in the task snapshot, one root's runtime
//! instances as a left-to-right DAG, and the catalog DAG source.
//!
//! The aggregation, edge derivation, pure layout, progress and optimistic
//! action state live in [`crate::workflow`] (pure and unit-tested); this file
//! only paints them. The roots list and the graph keep their scroll handles
//! across refreshes, and a live `task.updated` never moves the highlighted node
//! (docs §2, Working Memory).
//!
//! Nodes are live task instances, not catalog definitions: a `needs`
//! successor appears once the daemon creates it. The catalog DOT panel stays
//! collapsed by default because it describes the definition graph, not this
//! run.

use std::time::Duration;

use gpui::{
    canvas, div, point, prelude::*, px, Animation, AnimationExt, AnyElement, Context, Div,
    ElementId, Entity, FocusHandle, Hsla, IntoElement, PathBuilder, Render, ScrollHandle, Stateful,
    Window,
};

use crate::components;
use crate::inbox::StatusKind;
use crate::launch::{self, LaunchFailure};
use crate::routes::Route;
use crate::state::{ClientState, SnapshotStatus};
use crate::theme::Theme;
use crate::workflow::{
    self, WorkflowDetail, WorkflowGraphLayout, WorkflowGraphState, WorkflowNode, WorkflowRow,
};

/// The stable key of one laid-out root: used to memoize the pure layout and to
/// remember which node was last scrolled into view.
type GraphSignature = Vec<(uuid::Uuid, Option<uuid::Uuid>, Option<i64>)>;

/// One painted edge: start point, end point and whether the selected node is
/// the child (so the path is accent colored).
type PaintedEdge = ((f32, f32), (f32, f32), bool);

/// The Workflows surface.
pub struct WorkflowsView {
    state: Entity<ClientState>,
    roots_scroll: ScrollHandle,
    graph_scroll: ScrollHandle,
    dot_scroll: ScrollHandle,
    root_scrolled_to: Option<uuid::Uuid>,
    graph_scrolled_to: Option<(uuid::Uuid, uuid::Uuid)>,
    /// Memoized layout: animation frames never re-run the placement pass.
    graph_cache: Option<(GraphSignature, WorkflowGraphLayout)>,
    refresh_focus: FocusHandle,
    cancel_focus: FocusHandle,
    copy_focus: FocusHandle,
    dot_toggle_focus: FocusHandle,
    retry_focus: FocusHandle,
    graph_retry_focus: FocusHandle,
    error_copy_focus: FocusHandle,
    /// The catalog DOT panel starts collapsed so the runtime graph owns the
    /// body; the toggle still reveals the catalog source.
    show_dot: bool,
}

impl WorkflowsView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self {
            state,
            roots_scroll: ScrollHandle::new(),
            graph_scroll: ScrollHandle::new(),
            dot_scroll: ScrollHandle::new(),
            root_scrolled_to: None,
            graph_scrolled_to: None,
            graph_cache: None,
            refresh_focus: cx.focus_handle().tab_index(20).tab_stop(true),
            cancel_focus: cx.focus_handle().tab_index(21).tab_stop(true),
            retry_focus: cx.focus_handle().tab_index(22).tab_stop(true),
            error_copy_focus: cx.focus_handle().tab_index(23).tab_stop(true),
            copy_focus: cx.focus_handle().tab_index(24).tab_stop(true),
            dot_toggle_focus: cx.focus_handle().tab_index(25).tab_stop(true),
            graph_retry_focus: cx.focus_handle().tab_index(26).tab_stop(true),
            show_dot: false,
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
                    .child(hint(theme, "node"))
                    .child(components::keycap(theme, "↵"))
                    .child(hint(theme, "detail"))
                    .child(components::keycap(theme, "⇧↵"))
                    .child(hint(theme, "tasks"))
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

    /// The first-load skeleton: a few node boxes joined by placeholder edges.
    fn loading_body(&self, theme: &Theme, detail: &WorkflowDetail) -> Div {
        let node = |width: f32| {
            div()
                .w(px(workflow::NODE_WIDTH))
                .h(px(workflow::NODE_HEIGHT))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(theme.spacing.s1))
                .px(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(theme.colors.border)
                .bg(theme.colors.surface)
                .child(
                    div()
                        .w(px(width))
                        .h(px(8.0))
                        .rounded(px(theme.radii.r4))
                        .bg(theme.colors.border),
                )
                .child(
                    div()
                        .w(px(56.0))
                        .h(px(6.0))
                        .rounded(px(theme.radii.r4))
                        .bg(theme.colors.border),
                )
        };
        let connector = || {
            div()
                .w(px(workflow::COLUMN_GAP))
                .h(px(1.0))
                .flex_shrink_0()
                .bg(theme.colors.border)
        };

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
                    .child(components::section_label(theme, "GRAPH"))
                    .child(hint(theme, "Loading…")),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(0.0))
                    .p(px(workflow::GRAPH_MARGIN))
                    .child(node(96.0))
                    .child(connector())
                    .child(node(120.0))
                    .child(connector())
                    .child(node(80.0)),
            )
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

    /// The graph canvas: edges painted first, then the node cards above them in
    /// the same absolutely positioned scroll content.
    fn graph_body(
        &self,
        theme: &Theme,
        detail: &WorkflowDetail,
        layout: &WorkflowGraphLayout,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selected = detail.selected_id();
        let mut content = div()
            .relative()
            .w(px(layout.width.max(1.0)))
            .h(px(layout.height.max(1.0)))
            .child(self.edge_canvas(theme, layout, selected));

        for node in &layout.nodes {
            let Some(row) = detail.rows.iter().find(|row| row.id == node.id) else {
                continue;
            };
            content = content.child(self.node_card(
                theme,
                detail.root_id,
                node,
                row,
                selected == Some(row.id),
                cx,
            ));
        }

        div()
            .id("workflow-graph")
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .track_scroll(&self.graph_scroll)
            .child(content)
    }

    /// Paint every lineage edge. The selected node's incoming edge is accent
    /// colored; all others use the muted border token.
    fn edge_canvas(
        &self,
        theme: &Theme,
        layout: &WorkflowGraphLayout,
        selected: Option<uuid::Uuid>,
    ) -> AnyElement {
        let edges: Vec<PaintedEdge> = layout
            .edges
            .iter()
            .filter_map(|edge| {
                let parent = layout.node(edge.parent)?;
                let child = layout.node(edge.child)?;
                Some((
                    (
                        parent.x + workflow::NODE_WIDTH,
                        parent.y + workflow::NODE_HEIGHT / 2.0,
                    ),
                    (child.x, child.y + workflow::NODE_HEIGHT / 2.0),
                    selected == Some(edge.child),
                ))
            })
            .collect();
        let width = layout.width.max(1.0);
        let height = layout.height.max(1.0);
        let muted = theme.colors.border;
        let accent = theme.colors.accent;

        canvas(
            move |_bounds, _window, _cx| edges,
            move |_bounds, edges, window, _cx| {
                let mut muted_path = PathBuilder::stroke(px(1.5));
                let mut accent_path = PathBuilder::stroke(px(1.5));
                let mut has_muted = false;
                let mut has_accent = false;
                for ((start_x, start_y), (end_x, end_y), highlighted) in edges {
                    let path = if highlighted {
                        has_accent = true;
                        &mut accent_path
                    } else {
                        has_muted = true;
                        &mut muted_path
                    };
                    path.move_to(point(px(start_x), px(start_y)));
                    // A short ease from the parent's right edge into the
                    // child's left edge; the control point stays level with the
                    // parent so the fork reads left to right.
                    let control = point(px((start_x + end_x) / 2.0), px(start_y));
                    path.curve_to(point(px(end_x), px(end_y)), control);
                }
                if has_muted {
                    if let Ok(path) = muted_path.build() {
                        window.paint_path(path, muted);
                    }
                }
                if has_accent {
                    if let Ok(path) = accent_path.build() {
                        window.paint_path(path, accent);
                    }
                }
            },
        )
        .absolute()
        .left_0()
        .top_0()
        .w(px(width))
        .h(px(height))
        .into_any_element()
    }

    /// One task instance card. Status is never color-only: the icon and label
    /// always render, and the root/blocked/attempt chips are text.
    fn node_card(
        &self,
        theme: &Theme,
        root_id: uuid::Uuid,
        node: &WorkflowNode,
        row: &WorkflowRow,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let kind = row.status_kind();
        let color = status_color(theme, kind);
        let blocked = row.is_blocked();
        let is_root = row.id == root_id;
        let task_id = row.id;
        let border = if selected {
            Hsla::from(theme.colors.accent)
        } else if blocked {
            Hsla::from(theme.colors.muted).opacity(0.7)
        } else {
            Hsla::from(theme.colors.border)
        };
        let background = if selected {
            Hsla::from(theme.colors.accent).opacity(0.14)
        } else {
            Hsla::from(theme.colors.surface)
        };

        let pulse = row.is_active().then(|| pulse_dot(color, task_id));

        div()
            .id(ElementId::Uuid(task_id))
            .absolute()
            .left(px(node.x))
            .top(px(node.y))
            .w(px(workflow::NODE_WIDTH))
            .h(px(workflow::NODE_HEIGHT))
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(2.0))
            .px(px(theme.spacing.s2))
            .py(px(theme.spacing.s1))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(border)
            .when(blocked, |card| card.border_dashed())
            .bg(background)
            .cursor_pointer()
            .when(!selected, |card| {
                card.hover(|style| style.bg(Hsla::from(theme.colors.surface).opacity(0.9)))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .min_w_0()
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
                    .when(is_root, |line| line.child(node_chip(theme, "root"))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(3.0))
                    .min_w_0()
                    .when_some(pulse, |line, pulse| line.child(pulse))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(theme.font.xs))
                            .text_color(color)
                            .child(kind.icon()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(theme.font.xs))
                            .text_color(color)
                            .child(kind.label()),
                    )
                    .when(row.attempt > 1, |line| {
                        line.child(node_chip(theme, &format!("attempt {}", row.attempt)))
                    })
                    .when(blocked, |line| line.child(node_chip(theme, "blocked"))),
            )
            .when_some(row.summary.clone(), |card, summary| {
                card.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.danger)
                        .child(summary),
                )
            })
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.open_workflow_instance(task_id);
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
                "Choose a root on the left to see its instances as a left-to-right graph. Edges follow each task's parent; progress is counted from the instances the daemon returns, never estimated.",
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

    /// The pure layout, memoized until the row set or its edges change. The
    /// repeating status pulse requests animation frames but never re-renders
    /// this view, so the cache also keeps those frames off the placement pass.
    fn layout_for(&mut self, detail: &WorkflowDetail) -> WorkflowGraphLayout {
        let signature: GraphSignature = detail
            .rows
            .iter()
            .map(|row| {
                (
                    row.id,
                    row.parent_id,
                    row.created_at.map(|created| created.timestamp_millis()),
                )
            })
            .collect();
        if let Some((cached, layout)) = &self.graph_cache {
            if *cached == signature {
                return layout.clone();
            }
        }
        let layout = detail.layout();
        self.graph_cache = Some((signature, layout.clone()));
        layout
    }

    /// Keep the selected node inside the two-axis viewport after a keyboard
    /// move, a click or a root switch. Layout is in content coordinates, so the
    /// target offset is the minimal shift that reveals the box.
    fn ensure_selected_visible(&mut self, detail: &WorkflowDetail, layout: &WorkflowGraphLayout) {
        let Some(selected) = detail.selected_id() else {
            return;
        };
        let Some(node) = layout.node(selected) else {
            return;
        };
        if self.graph_scrolled_to == Some((detail.root_id, selected)) {
            return;
        }
        let bounds = self.graph_scroll.bounds();
        let viewport_width = f32::from(bounds.size.width);
        let viewport_height = f32::from(bounds.size.height);
        if viewport_width <= 0.0 || viewport_height <= 0.0 {
            // The first frame has no measured viewport yet; try again on the
            // next render instead of pinning a bogus offset.
            return;
        }
        self.graph_scrolled_to = Some((detail.root_id, selected));

        let max = self.graph_scroll.max_offset();
        let max_x = f32::from(max.width);
        let max_y = f32::from(max.height);
        let offset = self.graph_scroll.offset();
        let mut x = f32::from(offset.x);
        let mut y = f32::from(offset.y);
        let padding = 16.0;

        let visible_left = -x;
        let visible_top = -y;
        if node.x - padding < visible_left {
            x = -(node.x - padding);
        } else if node.x + workflow::NODE_WIDTH + padding > visible_left + viewport_width {
            x = -(node.x + workflow::NODE_WIDTH + padding - viewport_width);
        }
        if node.y - padding < visible_top {
            y = -(node.y - padding);
        } else if node.y + workflow::NODE_HEIGHT + padding > visible_top + viewport_height {
            y = -(node.y + workflow::NODE_HEIGHT + padding - viewport_height);
        }

        x = x.clamp(-max_x, 0.0);
        y = y.clamp(-max_y, 0.0);
        self.graph_scroll.set_offset(point(px(x), px(y)));
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
            let layout = self.layout_for(&detail);
            self.ensure_selected_visible(&detail, &layout);
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
                column = column.child(self.graph_body(theme, &detail, &layout, cx));
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

/// A tiny text chip on a node card ("root", "blocked", "attempt n").
fn node_chip(theme: &Theme, label: &str) -> Div {
    div()
        .flex_shrink_0()
        .max_w(px(96.0))
        .truncate()
        .px(px(3.0))
        .py(px(1.0))
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(theme.colors.border)
        .bg(theme.colors.bg)
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child(label.to_string())
}

/// The pulsing status dot for a live node.
///
/// The icon and label already carry the status, so reduced motion removes only
/// the pulse and keeps a fully readable static dot. The element id is stable per
/// task id, so a re-render does not restart the phase.
fn pulse_dot(color: gpui::Rgba, id: uuid::Uuid) -> AnyElement {
    let dot = div()
        .w(px(6.0))
        .h(px(6.0))
        .flex_shrink_0()
        .rounded_full()
        .bg(color);
    if crate::motion::prefers_reduced_motion() {
        return dot.into_any_element();
    }
    dot.with_animation(
        (ElementId::Uuid(id), "workflow-node-spin"),
        Animation::new(Duration::from_millis(1200)).repeat(),
        |dot, delta| {
            // A triangle wave: fade down and back up on every repeat.
            let triangle = 1.0 - (delta * 2.0 - 1.0).abs();
            dot.opacity(0.3 + 0.7 * triangle)
        },
    )
    .into_any_element()
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
