//! The task detail surface: a pinned header, three tabs and the permission
//! banner (docs/design/gpui-client.md §1, §2, §5 step 06).
//!
//! This is the client's core surface, so it follows three rules:
//!
//! - **Working Memory** — the header (name, status, agent, repo, attempt,
//!   elapsed, session id, usage) never scrolls away; only the tab body scrolls.
//! - **Peak-End** — a finished run ends with its outcome and usage in the
//!   header, and every tab has an explicit empty/error state with Retry.
//! - **Von Restorff** — a blocked run gets the only warning-colored surface
//!   (the permission banner), and Cancel is the only danger-colored action.
//!
//! The aggregation and feed grouping live in [`crate::detail`] (pure and
//! unit-tested); this file only paints them.

use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use chrono::Utc;
use favetto_wire::model::{AgentUsage, ContentRole, InputReply, MessageRole, Task, TaskRun};
use gpui::{
    div, list, prelude::*, px, AnyElement, Context, Div, ElementId, Entity, FocusHandle, FontStyle,
    FontWeight, Hsla, InteractiveText, IntoElement, ListAlignment, ListState, Render, ScrollHandle,
    ScrollWheelEvent, Stateful, StrikethroughStyle, StyledText, TextRun, TextStyle, UnderlineStyle,
    Window,
};
use uuid::Uuid;

use crate::components;
use crate::detail::{
    self, DetailTab, FeedItem, OutputText, PendingRequest, TaskDetail, ToolStatus,
};
use crate::inbox::{self, StatusKind};
use crate::layout::{container_query, LayoutMode};
use crate::markdown::syntax::{self, TokenKind};
use crate::markdown::{self, Align, Block, Inline, MarkdownDoc};
use crate::state::ClientState;
use crate::tasks;
use crate::theme::Theme;

/// Pixels from the bottom within which the viewport counts as "at the tail".
const FOLLOW_THRESHOLD: f32 = 24.0;

/// The detail surface.
pub struct DetailView {
    state: Entity<ClientState>,
    back_focus: FocusHandle,
    tab_focus: Vec<FocusHandle>,
    cancel_focus: FocusHandle,
    interrupt_focus: FocusHandle,
    retry_focus: FocusHandle,
    copy_focus: FocusHandle,
    workflow_focus: FocusHandle,
    session_focus: FocusHandle,
    follow_focus: FocusHandle,
    output_retry_focus: FocusHandle,
    events_retry_focus: FocusHandle,
    runs_retry_focus: FocusHandle,
    reply_once_focus: FocusHandle,
    reply_always_focus: FocusHandle,
    reply_reject_focus: FocusHandle,
    new_events_focus: FocusHandle,
    /// Copy action for whichever full-body error state is visible.
    error_copy_focus: FocusHandle,
    /// Persisted across renders so live events never jump the reader.
    output_scroll: ScrollHandle,
    /// Virtualized feed state: only the visible window is measured/painted.
    events_list: ListState,
    /// The grouped feed, rebuilt only when the event revision changes.
    events_feed: Rc<Vec<FeedRow>>,
    events_feed_key: Option<(Uuid, u64)>,
    /// Parsed Markdown for the message/reasoning rows, keyed by content and
    /// kept across feed revisions until a row disappears.
    markdown: markdown::MarkdownCache,
    markdown_task: Option<Uuid>,
    /// The content length at the last tail scroll, so auto-follow only reacts to
    /// growth (and never fights a deliberate scroll).
    output_rendered: usize,
}

impl DetailView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        let events_list = ListState::new(0, ListAlignment::Bottom, px(600.0));
        let follow_state = state.clone();
        events_list.set_scroll_handler(move |event, _window, cx| {
            let at_bottom = !event.is_scrolled;
            follow_state.update(cx, |state, cx| {
                if state.detail_follow() != at_bottom {
                    state.set_detail_follow(at_bottom);
                    cx.notify();
                }
            });
        });
        let view = Self {
            state,
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
            session_focus: cx.focus_handle().tab_index(51).tab_stop(true),
            follow_focus: cx.focus_handle().tab_index(62).tab_stop(true),
            output_retry_focus: cx.focus_handle().tab_index(63).tab_stop(true),
            events_retry_focus: cx.focus_handle().tab_index(64).tab_stop(true),
            runs_retry_focus: cx.focus_handle().tab_index(65).tab_stop(true),
            reply_once_focus: cx.focus_handle().tab_index(56).tab_stop(true),
            reply_always_focus: cx.focus_handle().tab_index(57).tab_stop(true),
            reply_reject_focus: cx.focus_handle().tab_index(58).tab_stop(true),
            workflow_focus: cx.focus_handle().tab_index(52).tab_stop(true),
            new_events_focus: cx.focus_handle().tab_index(66).tab_stop(true),
            error_copy_focus: cx.focus_handle().tab_index(67).tab_stop(true),
            output_scroll: ScrollHandle::new(),
            events_list,
            events_feed: Rc::new(Vec::new()),
            events_feed_key: None,
            markdown: markdown::MarkdownCache::default(),
            markdown_task: None,
            output_rendered: 0,
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

    /// Settle the follow state from the post-scroll viewport.
    ///
    /// The check is deferred to the end of the effect cycle, after the scroll
    /// container has applied the wheel delta, so it is independent of the
    /// platform's wheel direction and of listener ordering. It also catches
    /// scrollbar drags and keyboard scrolling, which never emit a wheel event.
    fn sync_follow(&self, handle: ScrollHandle, cx: &mut Context<Self>) {
        let state = self.state.clone();
        cx.defer(move |cx| {
            let at_bottom = at_bottom(&handle);
            state.update(cx, |state, cx| {
                let follow = state.detail_follow();
                if at_bottom && !follow {
                    state.set_detail_follow(true);
                    cx.notify();
                } else if !at_bottom && follow {
                    state.set_detail_follow(false);
                    cx.notify();
                }
            });
        });
    }

    fn on_output_wheel(
        &mut self,
        _event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_follow(self.output_scroll.clone(), cx);
    }

    fn follow_toggle(&self, theme: &Theme, follow: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        components::with_focus(
            div()
                .id("detail-follow")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s1))
                .h(px(theme.controls.min))
                .px(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(if follow {
                    Hsla::from(theme.colors.accent).opacity(0.5)
                } else {
                    Hsla::from(theme.colors.border)
                })
                .text_size(px(theme.font.xs))
                .text_color(if follow {
                    theme.colors.accent
                } else {
                    theme.colors.muted
                })
                .cursor_pointer()
                .track_focus(&self.follow_focus)
                .child(if follow { "◉" } else { "○" })
                .child(if follow { "Following" } else { "Follow tail" })
                .on_click(cx.listener(|this, _event, _window, cx| {
                    let follow = this.state.read(cx).detail_follow();
                    this.state.update(cx, |state, cx| {
                        state.set_detail_follow(!follow);
                        cx.notify();
                    });
                    if !follow {
                        // Resuming: jump straight to the newest content. The
                        // virtualized feed resets to the bottom on the next
                        // paint; the output handle scrolls immediately.
                        this.output_scroll.scroll_to_bottom();
                        this.events_list.reset(this.events_list.item_count());
                        this.output_rendered = 0;
                    }
                })),
            theme,
        )
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
        let elapsed = inbox::task_duration(task, Utc::now())
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
                    state.request_detail_action(tasks::TaskAction::Cancel, cx);
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
                    state.request_detail_action(tasks::TaskAction::Retry, cx);
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

        let session_row = data.session_id.map(|session_id| {
            let value = session_id.to_string();
            let copy = components::with_focus(
                div()
                    .id("detail-copy-session")
                    .px(px(theme.spacing.s1))
                    .rounded(px(theme.radii.r4))
                    .text_color(theme.colors.muted)
                    .cursor_pointer()
                    .track_focus(&self.session_focus)
                    .hover(|style| style.bg(theme.colors.bg))
                    .child("copy")
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.state.update(cx, |state, cx| {
                            state.copy_session_id(value.clone(), cx);
                            cx.notify();
                        });
                    })),
                theme,
            );
            div()
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .min_w_0()
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .child("session")
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family("monospace")
                        .text_color(theme.colors.text)
                        .child(session_id.to_string()),
                )
                .child(copy)
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
                            .child(status.icon())
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
                    .when_some(session_row, |row, session| {
                        row.child(div().flex_1().min_w_0().child(session))
                    })
                    .when(data.session_id.is_none(), |row| row.child(div().flex_1()))
                    .when_some(interrupt, |row, interrupt| row.child(interrupt))
                    .child(workflow)
                    .child(cancel)
                    .child(retry)
                    .child(copy),
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

    /// A small secondary button used by the header. The click handler comes in
    /// as a listener so each action keeps its own `cx`.
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

    /// The header's turn-interrupt control. It is disabled while a cancellation
    /// is in flight, so a double click cannot send a second `agents.cancel`.
    fn interrupt_button(
        &self,
        theme: &Theme,
        label: &'static str,
        enabled: bool,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> Stateful<Div> {
        components::with_focus(
            div()
                .id("detail-interrupt")
                .flex_shrink_0()
                .flex()
                .items_center()
                .h(px(theme.controls.min))
                .px(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(Hsla::from(theme.colors.border))
                .text_size(px(theme.font.xs))
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

    fn tabs(
        &self,
        theme: &Theme,
        tab: DetailTab,
        new_events: u64,
        follow: bool,
        cx: &mut Context<Self>,
    ) -> Div {
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
            let badge = (candidate == DetailTab::Events && new_events > 0).then(|| {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .min_w(px(16.0))
                    .h(px(16.0))
                    .px(px(3.0))
                    .rounded(px(theme.radii.r12))
                    .bg(Hsla::from(theme.colors.warning).opacity(0.2))
                    .text_color(theme.colors.warning)
                    .text_size(px(theme.font.xs))
                    .child(new_events.to_string())
            });
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
                    .when_some(badge, |button, badge| button.child(badge))
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.state.update(cx, |state, cx| {
                            state.set_detail_tab(candidate);
                            cx.notify();
                        });
                    })),
                theme,
            ));
        }

        bar.child(div().flex_1())
            .child(self.follow_toggle(theme, follow, cx))
    }

    /// The pinned permission banner. It names the request and gives exactly
    /// three outcomes (once / always when offered / reject); a screen-detected
    /// prompt with no correlation id explains that it must be answered in the
    /// terminal instead (docs §6, permission semantics).
    fn banner(
        &self,
        theme: &Theme,
        request: &PendingRequest,
        pending: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let color = theme.colors.warning;
        let can_reply = request.can_reply() && !pending;

        let mut options = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(theme.spacing.s1));
        if !request.options.is_empty() {
            options = options.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child("options"),
            );
            for option in &request.options {
                options = options.child(
                    div()
                        .px(px(theme.spacing.s1 + 2.0))
                        .py(px(1.0))
                        .rounded(px(theme.radii.r4))
                        .border_1()
                        .border_color(Hsla::from(color).opacity(0.35))
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.text)
                        .child(option.clone()),
                );
            }
        }

        let prompt = div()
            .flex()
            .flex_1()
            .min_w_0()
            .flex_col()
            .gap(px(theme.spacing.s1))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s1))
                    .text_size(px(theme.font.xs))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(color)
                    .child("⚠")
                    .child(format!("Awaiting {}", request.kind_label())),
            )
            .child(
                div()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(request.message.clone()),
            )
            .child(options)
            .when(!request.can_reply(), |prompt| {
                prompt.child(
                    div()
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.muted)
                        .child(
                            "This prompt was detected on the agent's screen; answer it in the agent's terminal.",
                        ),
                )
            })
            .when(pending, |prompt| {
                prompt.child(
                    div()
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.muted)
                        .child("Sending the reply…"),
                )
            });

        let once = self.reply_button(
            theme,
            "reply-once",
            "Allow once",
            false,
            can_reply,
            &self.reply_once_focus,
            request,
            InputReply::Once,
            cx,
        );
        let always = request.allow_always.then(|| {
            self.reply_button(
                theme,
                "reply-always",
                "Always allow",
                false,
                can_reply,
                &self.reply_always_focus,
                request,
                InputReply::Always,
                cx,
            )
        });
        let reject = self.reply_button(
            theme,
            "reply-reject",
            "Reject",
            true,
            can_reply,
            &self.reply_reject_focus,
            request,
            InputReply::Reject,
            cx,
        );

        div()
            .flex()
            .items_start()
            .gap(px(theme.spacing.s3))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s3))
            .border_b_1()
            .border_color(Hsla::from(color).opacity(0.45))
            .bg(Hsla::from(color).opacity(0.10))
            .child(prompt)
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(theme.spacing.s1))
                    .child(once)
                    .when_some(always, |column, always| column.child(always))
                    .child(reject),
            )
    }

    #[allow(clippy::too_many_arguments)]
    fn reply_button(
        &self,
        theme: &Theme,
        id: &'static str,
        label: &'static str,
        danger: bool,
        enabled: bool,
        focus: &FocusHandle,
        request: &PendingRequest,
        reply: InputReply,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let color = if danger {
            theme.colors.danger
        } else {
            theme.colors.text
        };
        let request = request.clone();
        components::with_focus(
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .h(px(theme.controls.primary))
                .px(px(theme.spacing.s3))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(if danger {
                    Hsla::from(theme.colors.danger).opacity(0.5)
                } else {
                    Hsla::from(theme.colors.border)
                })
                .when(enabled, |button| {
                    button.text_color(color).cursor_pointer().hover(|style| {
                        style.bg(if danger {
                            Hsla::from(theme.colors.danger).opacity(0.16)
                        } else {
                            Hsla::from(theme.colors.accent).opacity(0.16)
                        })
                    })
                })
                .when(!enabled, |button| {
                    button
                        .text_color(theme.colors.muted)
                        .opacity(0.55)
                        .cursor_default()
                })
                .text_size(px(theme.font.xs))
                .when(enabled, |button| button.track_focus(focus))
                .child(label)
                .when(enabled, |button| {
                    button.on_click(cx.listener(move |this, _event, _window, cx| {
                        this.state.update(cx, |state, cx| {
                            state.reply_to_pending(&request, reply.clone(), cx);
                            cx.notify();
                        });
                    }))
                }),
            theme,
        )
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

    fn error_body(
        &self,
        theme: &Theme,
        data: ErrorBody<'_>,
        retry: impl Fn(&mut DetailView, &mut Context<DetailView>) + 'static,
        cx: &mut Context<Self>,
    ) -> Div {
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
                    retry(this, cx);
                    cx.notify();
                })),
            theme,
        );
        let copy =
            components::copy_button(theme, "detail-error-copy", "Copy error", data.copy_focus);
        let message = data.message.to_string();
        let copy = copy.on_click(cx.listener(move |this, _event, _window, cx| {
            this.state.update(cx, |state, cx| {
                state.copy_text(message.clone(), "the error", cx);
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

    fn error_banner(&self, theme: &Theme, message: &str, cx: &mut Context<Self>) -> Div {
        let copy_text = message.to_string();
        let copy = components::copy_button(
            theme,
            "detail-error-copy-banner",
            "Copy error",
            &self.error_copy_focus,
        )
        .on_click(cx.listener(move |_this, _event, _window, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_text.clone()));
        }));
        div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
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
                    .min_w(px(0.0))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.text)
                    .child(format!("Refresh failed: {message}")),
            )
            .child(copy)
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

    fn output_body(
        &mut self,
        theme: &Theme,
        output: Option<OutputText>,
        loading: bool,
        task_error: Option<String>,
        terminal: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if loading && output.is_none() {
            return self.loading(theme, "OUTPUT").into_any_element();
        }
        if let (None, Some(message)) = (&output, &task_error) {
            return self
                .error_body(
                    theme,
                    ErrorBody {
                        label: "OUTPUT",
                        title: "The run output could not load",
                        message,
                        focus: &self.output_retry_focus,
                        copy_focus: &self.error_copy_focus,
                    },
                    |this, cx| {
                        this.state.update(cx, |state, cx| {
                            state.retry_detail_task();
                            cx.notify();
                        });
                    },
                    cx,
                )
                .into_any_element();
        }
        let Some(output) = output else {
            let title = if terminal {
                "The run finished without persisted output"
            } else {
                "No output yet"
            };
            return self.empty_body(
                theme,
                "≡",
                "OUTPUT",
                title,
                if terminal {
                    "The daemon has no output blob for this run. The Events and Runs tabs keep the structured record."
                } else {
                    "The daemon persists output when the run produces it. Follow the Events tab while it runs."
                },
            )
            .into_any_element();
        };

        let text = output.text;
        let truncated = output.truncated;

        let mut column = div()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s3));
        if let Some(message) = task_error {
            column = column.child(self.error_banner(theme, &message, cx));
        }
        if truncated {
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .px(px(theme.spacing.s2))
                    .py(px(theme.spacing.s1))
                    .rounded(px(theme.radii.r4))
                    .border_1()
                    .border_color(Hsla::from(theme.colors.warning).opacity(0.45))
                    .bg(Hsla::from(theme.colors.warning).opacity(0.10))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.text)
                    .child("⚠")
                    .child("The daemon truncated this output; the text below is what it kept."),
            );
        }
        column = column.child(
            div()
                .id("detail-output")
                .font_family("monospace")
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.text)
                .whitespace_normal()
                .child(text.clone()),
        );

        // Follow the tail only when the content grew, never on every render.
        if self.state.read(cx).detail_follow() && self.output_rendered != text.len() {
            self.output_scroll.scroll_to_bottom();
        }
        self.output_rendered = text.len();
        self.sync_follow(self.output_scroll.clone(), cx);

        div()
            .id("detail-output-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.output_scroll)
            .on_scroll_wheel(cx.listener(Self::on_output_wheel))
            .child(column)
            .into_any_element()
    }

    /// Rebuild the grouped feed only when its source revision changes, and keep
    /// the virtualized list's item count in sync. A paused reader keeps their
    /// logical position; a following reader stays at the tail.
    fn feed_for(
        &mut self,
        task_id: Uuid,
        revision: u64,
        cx: &mut Context<Self>,
    ) -> Rc<Vec<FeedRow>> {
        let key = (task_id, revision);
        if self.events_feed_key != Some(key) {
            let feed = {
                let state = self.state.read(cx);
                state
                    .detail()
                    .filter(|detail| detail.task_id == task_id)
                    .map(TaskDetail::feed)
                    .unwrap_or_default()
            };

            if self.markdown_task != Some(task_id) {
                self.markdown = markdown::MarkdownCache::default();
                self.markdown_task = Some(task_id);
            }
            self.markdown.begin_refresh();
            let markdown = &mut self.markdown;
            let rows = feed
                .into_iter()
                .map(|item| {
                    let body = match &item {
                        FeedItem::Message { text, .. } | FeedItem::Reasoning { text, .. } => {
                            Some(markdown.doc(text))
                        }
                        _ => None,
                    };
                    FeedRow { item, body }
                })
                .collect();
            self.markdown.end_refresh();

            self.events_feed = Rc::new(rows);
            self.events_feed_key = Some(key);

            let follow = self.state.read(cx).detail_follow();
            let len = self.events_feed.len();
            let top = (!follow).then(|| self.events_list.logical_scroll_top());
            self.events_list.reset(len);
            if let Some(top) = top {
                self.events_list.scroll_to(top);
            }
        }
        self.events_feed.clone()
    }

    fn events_body(&mut self, theme: &Theme, data: EventsBody, cx: &mut Context<Self>) -> Div {
        let EventsBody {
            feed,
            loading,
            loaded,
            error: events_error,
            session,
            ignored,
        } = data;
        if loading && feed.is_empty() && !loaded {
            return self.loading(theme, "EVENTS");
        }
        if feed.is_empty() && events_error.is_some() {
            return self.error_body(
                theme,
                ErrorBody {
                    label: "EVENTS",
                    title: "The event feed could not load",
                    message: events_error.as_deref().unwrap_or_default(),
                    focus: &self.events_retry_focus,
                    copy_focus: &self.error_copy_focus,
                },
                |this, cx| {
                    this.state.update(cx, |state, cx| {
                        state.retry_detail_events();
                        cx.notify();
                    });
                },
                cx,
            );
        }
        if feed.is_empty() {
            if ignored > 0 {
                return self.empty_body(
                    theme,
                    "≋",
                    "EVENTS",
                    "Events from a newer daemon",
                    &format!(
                        "{ignored} event{} used a kind this client does not render yet. Upgrade the client to see them.",
                        if ignored == 1 { "" } else { "s" }
                    ),
                );
            }
            let (title, body) = if session.is_some() {
                (
                    "No events yet",
                    "The session is attached but has not reported a structured event yet. Live events land here as they arrive.",
                )
            } else {
                (
                    "No structured feed",
                    "This session does not report structured events (a PTY-backed agent). The Output tab is its durable view.",
                )
            };
            return self.empty_body(theme, "≋", "EVENTS", title, body);
        }

        let new_events = self.state.read(cx).detail_new_events();

        let list_theme = *theme;
        let list_feed = feed.clone();
        let list = list(self.events_list.clone(), move |index, window, _cx| {
            let Some(row) = list_feed.get(index) else {
                return div().into_any_element();
            };
            let base = window.text_style();
            div()
                .px(px(list_theme.spacing.s3))
                .pb(px(list_theme.spacing.s2))
                .child(feed_item(&list_theme, &base, index, row))
                .into_any_element()
        })
        .flex_1()
        .min_h_0();

        let pill = (new_events > 0).then(|| {
            components::with_focus(
                div()
                    .id("detail-new-events")
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s1))
                    .px(px(theme.spacing.s2))
                    .py(px(theme.spacing.s1))
                    .rounded(px(theme.radii.r12))
                    .border_1()
                    .border_color(Hsla::from(theme.colors.accent).opacity(0.5))
                    .bg(theme.colors.surface)
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.accent)
                    .cursor_pointer()
                    .shadow_lg()
                    .track_focus(&self.new_events_focus)
                    .child(format!(
                        "{new_events} new event{}",
                        if new_events == 1 { "" } else { "s" }
                    ))
                    .child("↓")
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.state.update(cx, |state, cx| {
                            state.set_detail_follow(true);
                            cx.notify();
                        });
                        this.events_list.reset(this.events_list.item_count());
                    })),
                theme,
            )
        });

        let mut container = div().relative().flex_1().min_h_0().flex().flex_col();
        if let Some(message) = events_error {
            container = container.child(
                div()
                    .px(px(theme.spacing.s3))
                    .pt(px(theme.spacing.s2))
                    .child(self.error_banner(theme, &message, cx)),
            );
        }
        if ignored > 0 {
            container = container.child(
                div()
                    .px(px(theme.spacing.s3))
                    .pt(px(theme.spacing.s2))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(format!(
                        "{ignored} event{} with a kind this client does not render yet were skipped.",
                        if ignored == 1 { "" } else { "s" }
                    )),
            );
        }
        container.child(list).when_some(pill, |container, pill| {
            container.child(
                div()
                    .absolute()
                    .right(px(theme.spacing.s3))
                    .bottom(px(theme.spacing.s3))
                    .child(pill),
            )
        })
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
                    |this, cx| {
                        this.state.update(cx, |state, cx| {
                            state.retry_detail_runs();
                            cx.notify();
                        });
                    },
                    cx,
                )
                .into_any_element();
        }
        if runs.is_empty() {
            return self.empty_body(
                theme,
                "↻",
                "RUNS",
                "No attempts recorded",
                "The daemon records a run row for every execution attempt. A queued task gets its first row when it starts.",
            )
            .into_any_element();
        }

        let now = Utc::now();
        let mut column = div()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s2));
        if let Some(message) = runs_error {
            column = column.child(self.error_banner(theme, &message, cx));
        }
        column = column.child(runs_header(theme));
        for run in runs.iter().rev() {
            column = column.child(run_row(theme, run, current_attempt, now));
        }
        div()
            .id("detail-runs-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(column)
            .into_any_element()
    }
}

/// The identity cluster the pinned header renders. Grouped so the header helper
/// stays readable and under the argument limit.
struct HeaderData<'a> {
    task: &'a Task,
    agent: Option<&'a str>,
    session_id: Option<&'a str>,
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

/// Everything the Events tab body needs, extracted from the state in one go.
struct EventsBody {
    feed: Rc<Vec<FeedRow>>,
    loading: bool,
    loaded: bool,
    error: Option<String>,
    session: Option<String>,
    ignored: usize,
}

/// One rendered feed row plus the parsed Markdown for prose/reasoning items.
struct FeedRow {
    item: FeedItem,
    body: Option<Rc<MarkdownDoc>>,
}

/// Everything the Runs tab body needs, extracted from the state in one go.
struct RunsBody<'a> {
    runs: &'a [TaskRun],
    current_attempt: u32,
    loading: bool,
    loaded: bool,
    error: Option<String>,
}

impl Render for DetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.state.read(cx).theme(window.appearance());
        let narrow = container_query(window.viewport_size().width) == LayoutMode::Narrow;

        let (task, agent, session_id, usage, tab, follow, new_events, pending) = {
            let state = self.state.read(cx);
            (
                state.selected_task().cloned(),
                state.detail_agent(),
                state.detail_session_id(),
                state.detail_usage(),
                state.detail_tab(),
                state.detail_follow(),
                state.detail_new_events(),
                state.detail_pending_request(),
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
                    "Open a run in the list to see its status, agent and attempt here. Output, Events and Runs stay one tab away.",
                    "Open a row to inspect it",
                ));
        };

        let header = self.header(
            theme,
            HeaderData {
                task: &task,
                agent: agent.as_deref(),
                session_id: session_id.as_deref(),
                usage: usage.as_ref(),
            },
            narrow,
            cx,
        );
        let banner = pending.as_ref().map(|request| {
            let in_flight = self
                .state
                .read(cx)
                .detail()
                .is_some_and(|detail| detail.reply_in_flight(request.request_id.as_deref()));
            self.banner(theme, request, in_flight, cx)
        });
        let tabs = self.tabs(theme, tab, new_events, follow, cx);
        let current_attempt = task.attempt;

        let body = match tab {
            DetailTab::Output => {
                let (output, loading, error, terminal) = {
                    let state = self.state.read(cx);
                    let detail = state.detail();
                    (
                        detail.and_then(TaskDetail::output),
                        detail.is_some_and(|detail| detail.task_loading && detail.task.is_none()),
                        detail.and_then(|detail| detail.task_error.clone()),
                        detail.is_some_and(TaskDetail::is_terminal),
                    )
                };
                self.output_body(theme, output, loading, error, terminal, cx)
            }
            DetailTab::Events => {
                let (revision, loading, loaded, error, session, ignored) = {
                    let state = self.state.read(cx);
                    let detail = state.detail();
                    (
                        detail.map(|detail| detail.events_revision).unwrap_or(0),
                        detail.is_some_and(|detail| detail.events_loading),
                        detail.is_some_and(|detail| detail.events_loaded),
                        detail.and_then(|detail| detail.events_error.clone()),
                        detail.and_then(|detail| detail.events_session.clone()),
                        detail.map(|detail| detail.events_ignored).unwrap_or(0),
                    )
                };
                let feed = self.feed_for(task.id, revision, cx);
                self.events_body(
                    theme,
                    EventsBody {
                        feed,
                        loading,
                        loaded,
                        error,
                        session,
                        ignored,
                    },
                    cx,
                )
                .into_any_element()
            }
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
            .when_some(banner, |detail, banner| detail.child(banner))
            .child(tabs)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
    }
}

impl DetailView {
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

fn role_label(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

fn content_role_label(role: ContentRole) -> &'static str {
    match role {
        ContentRole::User => "user",
        ContentRole::Assistant => "assistant",
        ContentRole::Thought => "thinking",
    }
}

fn outcome_label(outcome: favetto_wire::model::IdleOutcome) -> &'static str {
    match outcome {
        favetto_wire::model::IdleOutcome::Succeeded => "succeeded",
        favetto_wire::model::IdleOutcome::Failed => "failed",
        favetto_wire::model::IdleOutcome::Interrupted => "interrupted",
    }
}

/// One feed item. Message and reasoning blocks can be long, so the text wraps
/// inside the narrow detail column; nothing here interprets ANSI. Prose and
/// reasoning bodies render as Markdown, while every other row keeps its
/// existing card/row treatment.
fn feed_item(theme: &Theme, base: &TextStyle, index: usize, row: &FeedRow) -> Div {
    match &row.item {
        FeedItem::Message { role, text } => {
            let header = div()
                .text_size(px(theme.font.xs))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(match role {
                    MessageRole::Assistant => theme.colors.accent,
                    MessageRole::User => theme.colors.success,
                    MessageRole::System | MessageRole::Tool => theme.colors.muted,
                })
                .child(role_label(*role));
            let body = match row.body.as_deref() {
                Some(doc) => markdown_body(theme, base, Some(doc), Tone::Prose, index),
                None => div()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(text.clone()),
            };
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(header)
                .child(body)
        }
        FeedItem::Reasoning { text, streaming } => {
            let header = div()
                .flex()
                .items_center()
                .gap(px(theme.spacing.s1))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .child("reasoning")
                .when(*streaming, |row| row.child("…"));
            let body = match row.body.as_deref() {
                Some(doc) => markdown_body(theme, base, Some(doc), Tone::Reasoning, index),
                None => div()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.muted)
                    .italic()
                    .child(text.clone()),
            };
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(header)
                .child(body)
        }
        FeedItem::ContentBlock { role, content } => div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px(px(theme.spacing.s2))
            .py(px(theme.spacing.s1))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(Hsla::from(theme.colors.border).opacity(0.45))
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.colors.muted)
                    .child(content_role_label(*role)),
            )
            .child(
                div()
                    .font_family("monospace")
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(content.placeholder()),
            ),
        FeedItem::Tool {
            name,
            input,
            status,
            output,
            ..
        } => {
            let (color, border) = match status {
                ToolStatus::Running => (theme.colors.accent, theme.colors.accent),
                ToolStatus::Ok => (theme.colors.success, theme.colors.border),
                ToolStatus::Error => (theme.colors.danger, theme.colors.danger),
            };
            let name = if name.is_empty() {
                "tool"
            } else {
                name.as_str()
            };
            let mut card = div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .px(px(theme.spacing.s2))
                .py(px(theme.spacing.s1))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(Hsla::from(border).opacity(0.45))
                .bg(Hsla::from(border).opacity(0.06))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(theme.spacing.s1))
                        .child(
                            div()
                                .text_size(px(theme.font.xs))
                                .text_color(color)
                                .child(status.icon()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family("monospace")
                                .text_size(px(theme.font.xs))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.colors.text)
                                .child(name.to_string()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(theme.font.xs))
                                .text_color(color)
                                .child(status.label()),
                        ),
                );
            if let Some(summary) = detail::compact_json(input) {
                card = card.child(
                    div()
                        .font_family("monospace")
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.muted)
                        .child(format!("→ {summary}")),
                );
            }
            if let Some(output) = output.as_ref().and_then(detail::compact_json) {
                card = card.child(
                    div()
                        .font_family("monospace")
                        .text_size(px(theme.font.xs))
                        .text_color(if *status == ToolStatus::Error {
                            theme.colors.danger
                        } else {
                            theme.colors.muted
                        })
                        .child(output),
                );
            }
            card
        }
        FeedItem::Usage(usage) => div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s1))
            .text_size(px(theme.font.xs))
            .text_color(theme.colors.muted)
            .child("⚡")
            .child(
                detail::usage_summary(Some(usage)).unwrap_or_else(|| "usage reported".to_string()),
            ),
        FeedItem::Error(message) => div()
            .flex()
            .items_start()
            .gap(px(theme.spacing.s1))
            .px(px(theme.spacing.s2))
            .py(px(theme.spacing.s1))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(Hsla::from(theme.colors.danger).opacity(0.45))
            .bg(Hsla::from(theme.colors.danger).opacity(0.10))
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.danger)
                    .child("⚠"),
            )
            .child(
                div()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(message.clone()),
            ),
        FeedItem::Boundary(outcome) => div()
            .py(px(theme.spacing.s1))
            .text_size(px(theme.font.xs))
            .text_color(theme.colors.muted)
            .text_center()
            .child(format!("— turn {}", outcome_label(*outcome))),
        FeedItem::InputRequest { request, resolved } => div()
            .flex()
            .items_start()
            .gap(px(theme.spacing.s1))
            .px(px(theme.spacing.s2))
            .py(px(theme.spacing.s1))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(Hsla::from(theme.colors.warning).opacity(0.4))
            .bg(Hsla::from(theme.colors.warning).opacity(0.08))
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.warning)
                    .child(if *resolved { "✓" } else { "⚠" }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(theme.font.xs))
                    .text_color(if *resolved {
                        theme.colors.muted
                    } else {
                        theme.colors.text
                    })
                    .child(format!(
                        "{} · {}",
                        if *resolved { "answered" } else { "waiting" },
                        request.message
                    )),
            ),
        FeedItem::Session { title } => div()
            .text_size(px(theme.font.xs))
            .text_color(theme.colors.muted)
            .child(match title {
                Some(title) => format!("session · {title}"),
                None => "session attached".to_string(),
            }),
        FeedItem::Title(title) => div()
            .text_size(px(theme.font.xs))
            .text_color(theme.colors.muted)
            .child(format!("session · {title}")),
    }
}

/// Which visual treatment a Markdown body uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Prose,
    Reasoning,
}

/// How many element ids each feed row may consume before the next row starts.
const MARKDOWN_ROW_STRIDE: u64 = 100_000;

fn markdown_body(
    theme: &Theme,
    base: &TextStyle,
    doc: Option<&MarkdownDoc>,
    tone: Tone,
    row: usize,
) -> Div {
    let mut ids = row as u64 * MARKDOWN_ROW_STRIDE;
    match doc {
        Some(doc) => render_blocks(theme, base, &doc.blocks, tone, 0, &mut ids),
        None => div(),
    }
}

fn render_blocks(
    theme: &Theme,
    base: &TextStyle,
    blocks: &[Block],
    tone: Tone,
    depth: usize,
    ids: &mut u64,
) -> Div {
    let mut column = div().flex().flex_col().gap(px(theme.spacing.s2)).min_w_0();
    for block in blocks {
        column = column.child(render_block(theme, base, block, tone, depth, ids));
    }
    column
}

fn render_block(
    theme: &Theme,
    base: &TextStyle,
    block: &Block,
    tone: Tone,
    depth: usize,
    ids: &mut u64,
) -> Div {
    match block {
        Block::Heading { level, content } => {
            let (size, weight, color) = heading_style(theme, *level);
            let mut heading = div().min_w_0().child(prose_colored(
                theme, base, content, tone, size, weight, color, ids,
            ));
            if *level <= 2 {
                heading = heading.pt(px(theme.spacing.s1));
            }
            heading
        }
        Block::Paragraph(content) => div().min_w_0().child(prose(
            theme,
            base,
            content,
            tone,
            theme.font.sm,
            base.font_weight,
            ids,
        )),
        Block::Code { language, code } => render_code_block(theme, base, language, code),
        Block::List {
            ordered,
            start,
            items,
        } => render_list(theme, base, *ordered, *start, items, tone, depth, ids),
        Block::Quote(blocks) => div()
            .flex()
            .flex_col()
            .min_w_0()
            .pl(px(theme.spacing.s2))
            .border_l_2()
            .border_color(theme.colors.border)
            .child(render_blocks(
                theme,
                base,
                blocks,
                Tone::Reasoning,
                depth + 1,
                ids,
            )),
        Block::Rule => div()
            .w_full()
            .py(px(theme.spacing.s1))
            .child(div().w_full().h(px(1.0)).bg(theme.colors.border)),
        Block::Table { align, head, rows } => {
            render_table(theme, base, align, head, rows, tone, depth, ids)
        }
    }
}

fn heading_style(theme: &Theme, level: u8) -> (f32, FontWeight, Hsla) {
    let weight = match level {
        1 | 2 => FontWeight::BOLD,
        3 | 5 => FontWeight::SEMIBOLD,
        _ => FontWeight::MEDIUM,
    };
    let color = if level >= 5 {
        Hsla::from(theme.colors.muted)
    } else {
        Hsla::from(theme.colors.text)
    };
    (theme.markdown_heading_size(level), weight, color)
}

fn prose(
    theme: &Theme,
    base: &TextStyle,
    content: &[Inline],
    tone: Tone,
    size: f32,
    weight: FontWeight,
    ids: &mut u64,
) -> AnyElement {
    let color = match tone {
        Tone::Prose => Hsla::from(theme.colors.text),
        Tone::Reasoning => Hsla::from(theme.colors.muted),
    };
    prose_colored(theme, base, content, tone, size, weight, color, ids)
}

#[allow(clippy::too_many_arguments)]
fn prose_colored(
    theme: &Theme,
    base: &TextStyle,
    content: &[Inline],
    tone: Tone,
    size: f32,
    weight: FontWeight,
    color: Hsla,
    ids: &mut u64,
) -> AnyElement {
    let inline = markdown::flatten(content);
    let id = *ids;
    *ids = (*ids).wrapping_add(1);
    inline_styled(theme, base, &inline, tone, size, weight, color, id)
}

#[allow(clippy::too_many_arguments)]
fn inline_styled(
    theme: &Theme,
    base: &TextStyle,
    inline: &markdown::InlineText,
    tone: Tone,
    size: f32,
    weight: FontWeight,
    color: Hsla,
    id: u64,
) -> AnyElement {
    let (runs, links) = inline_runs(theme, base, inline, tone, size, weight, color);
    let styled = StyledText::new(inline.text.clone()).with_runs(runs);
    if links.is_empty() {
        return div()
            .min_w_0()
            .whitespace_normal()
            .child(styled)
            .into_any_element();
    }

    let ranges: Vec<Range<usize>> = links.iter().map(|(range, _)| range.clone()).collect();
    let urls: Vec<String> = links.into_iter().map(|(_, url)| url).collect();
    let text = InteractiveText::new(ElementId::NamedInteger("md-prose".into(), id), styled)
        .on_click(ranges, move |index, _window, cx| {
            if let Some(url) = urls.get(index) {
                cx.open_url(url);
            }
        });
    div()
        .min_w_0()
        .whitespace_normal()
        .child(text)
        .into_any_element()
}

fn inline_runs(
    theme: &Theme,
    base: &TextStyle,
    inline: &markdown::InlineText,
    tone: Tone,
    size: f32,
    weight: FontWeight,
    color: Hsla,
) -> (Vec<TextRun>, Vec<(Range<usize>, String)>) {
    let mut runs = Vec::new();
    let mut links = Vec::new();
    let mut cursor = 0usize;

    for span in &inline.spans {
        if span.range.start > cursor {
            runs.push(
                base_style(base, tone, size, weight, color).to_run(span.range.start - cursor),
            );
        }
        let start = span.range.start.max(cursor);
        let end = span.range.end.max(start);
        cursor = end;

        let mut style = base_style(base, tone, size, weight, color);
        if span.style.code {
            style.font_family = "monospace".into();
            style.background_color = Some(Hsla::from(theme.colors.border).opacity(0.35));
        }
        if span.style.strong {
            style.font_weight = FontWeight::SEMIBOLD;
        }
        if span.style.emphasis {
            style.font_style = FontStyle::Italic;
        }
        if span.style.strike {
            style.strikethrough = Some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(Hsla::from(theme.colors.muted)),
            });
        }
        if let Some(url) = &span.style.link {
            style.color = Hsla::from(theme.colors.accent);
            style.underline = Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(Hsla::from(theme.colors.accent)),
                wavy: false,
            });
            if is_openable(url) {
                links.push((start..end, url.clone()));
            }
        }
        runs.push(style.to_run(end - start));
    }

    if cursor < inline.text.len() {
        runs.push(base_style(base, tone, size, weight, color).to_run(inline.text.len() - cursor));
    }
    if runs.is_empty() && !inline.text.is_empty() {
        runs.push(base_style(base, tone, size, weight, color).to_run(inline.text.len()));
    }
    debug_assert_eq!(
        runs.iter().map(|run| run.len).sum::<usize>(),
        inline.text.len()
    );
    (runs, links)
}

fn base_style(
    base: &TextStyle,
    tone: Tone,
    size: f32,
    weight: FontWeight,
    color: Hsla,
) -> TextStyle {
    let mut style = base.clone();
    style.font_size = px(size).into();
    style.font_weight = weight;
    style.color = color;
    if matches!(tone, Tone::Reasoning) {
        style.font_style = FontStyle::Italic;
    }
    style
}

fn is_openable(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn render_code_block(theme: &Theme, base: &TextStyle, language: &str, code: &str) -> Div {
    let mut container = div()
        .flex()
        .flex_col()
        .min_w_0()
        .overflow_hidden()
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(Hsla::from(theme.colors.border).opacity(0.45))
        .bg(Hsla::from(theme.colors.border).opacity(0.12));

    let label = syntax::language_name(language);
    if !label.is_empty() {
        container = container.child(
            div()
                .px(px(theme.spacing.s2))
                .py(px(theme.spacing.s1))
                .border_b_1()
                .border_color(Hsla::from(theme.colors.border).opacity(0.35))
                .font_family("monospace")
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .child(label),
        );
    }

    let tokens = syntax::highlight(language, code);
    let mut text = String::new();
    let mut runs = Vec::new();
    for token in tokens {
        let mut style = base.clone();
        style.font_family = "monospace".into();
        style.font_size = px(theme.font.xs).into();
        style.color = Hsla::from(syntax_color(theme, token.kind));
        text.push_str(&token.text);
        runs.push(style.to_run(token.text.len()));
    }

    if !text.is_empty() {
        container = container.child(
            div()
                .min_w_0()
                .whitespace_normal()
                .p(px(theme.spacing.s2))
                .child(StyledText::new(text).with_runs(runs)),
        );
    }
    container
}

fn syntax_color(theme: &Theme, kind: TokenKind) -> gpui::Rgba {
    match kind {
        TokenKind::Plain => theme.colors.text,
        TokenKind::Keyword => theme.syntax.keyword,
        TokenKind::String => theme.syntax.string,
        TokenKind::Number => theme.syntax.number,
        TokenKind::Comment => theme.syntax.comment,
        TokenKind::Type => theme.syntax.type_name,
        TokenKind::Function => theme.syntax.function,
        TokenKind::Key => theme.syntax.key,
        TokenKind::Operator | TokenKind::Punctuation => theme.syntax.punctuation,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_list(
    theme: &Theme,
    base: &TextStyle,
    ordered: bool,
    start: u64,
    items: &[markdown::ListItem],
    tone: Tone,
    depth: usize,
    ids: &mut u64,
) -> Div {
    let mut column = div().flex().flex_col().gap(px(theme.spacing.s1)).min_w_0();
    if depth > 0 {
        column = column.pl(px(theme.spacing.s3));
    }

    for (offset, item) in items.iter().enumerate() {
        let (marker, color) = match item.task {
            Some(true) => ("☑".to_string(), theme.colors.accent),
            Some(false) => ("☐".to_string(), theme.colors.accent),
            None if ordered => (format!("{}.", start + offset as u64), theme.colors.muted),
            None => ("•".to_string(), theme.colors.muted),
        };
        let content = if item.blocks.is_empty() {
            div()
        } else {
            render_blocks(theme, base, &item.blocks, tone, depth + 1, ids)
        };
        column = column.child(
            div()
                .flex()
                .items_start()
                .gap(px(theme.spacing.s2))
                .min_w_0()
                .child(
                    div()
                        .w(px(18.0))
                        .flex_shrink_0()
                        .text_size(px(theme.font.sm))
                        .text_color(color)
                        .child(marker),
                )
                .child(div().flex_1().min_w_0().child(content)),
        );
    }
    column
}

#[allow(clippy::too_many_arguments)]
fn render_table(
    theme: &Theme,
    base: &TextStyle,
    align: &[Align],
    head: &[Vec<Inline>],
    rows: &[Vec<Vec<Inline>>],
    tone: Tone,
    depth: usize,
    ids: &mut u64,
) -> Div {
    let border = Hsla::from(theme.colors.border).opacity(0.45);
    let mut container = div()
        .flex()
        .flex_col()
        .min_w_0()
        .overflow_hidden()
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(border);

    if !head.is_empty() {
        container = container.child(render_table_row(
            theme, base, head, align, tone, depth, ids, true,
        ));
    }
    for (index, row) in rows.iter().enumerate() {
        let mut rendered = render_table_row(theme, base, row, align, tone, depth, ids, false);
        if index > 0 || !head.is_empty() {
            rendered = rendered
                .border_t_1()
                .border_color(Hsla::from(theme.colors.border).opacity(0.35));
        }
        container = container.child(rendered);
    }
    container
}

#[allow(clippy::too_many_arguments)]
fn render_table_row(
    theme: &Theme,
    base: &TextStyle,
    cells: &[Vec<Inline>],
    align: &[Align],
    tone: Tone,
    _depth: usize,
    ids: &mut u64,
    header: bool,
) -> Div {
    let mut row = div()
        .flex()
        .w_full()
        .min_w_0()
        .when(header, |row| row.bg(theme.colors.surface));
    let last = cells.len().saturating_sub(1);
    for (index, cell) in cells.iter().enumerate() {
        let alignment = align.get(index).copied().unwrap_or(Align::None);
        let content = if cell.is_empty() {
            div().into_any_element()
        } else {
            let color = if header {
                Hsla::from(theme.colors.text)
            } else if matches!(tone, Tone::Reasoning) {
                Hsla::from(theme.colors.muted)
            } else {
                Hsla::from(theme.colors.text)
            };
            let weight = if header {
                FontWeight::SEMIBOLD
            } else {
                base.font_weight
            };
            prose_colored(theme, base, cell, tone, theme.font.xs, weight, color, ids)
        };
        let mut cell = div()
            .flex_1()
            .min_w_0()
            .px(px(theme.spacing.s1))
            .py(px(2.0))
            .when(index < last, |cell| {
                cell.border_r_1()
                    .border_color(Hsla::from(theme.colors.border).opacity(0.35))
            })
            .child(content);
        cell = match alignment {
            Align::Center => cell.text_center(),
            Align::Right => cell.text_right(),
            Align::Left | Align::None => cell.text_left(),
        };
        row = row.child(cell);
    }
    row
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

fn run_row(theme: &Theme, run: &TaskRun, current_attempt: u32, now: chrono::DateTime<Utc>) -> Div {
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
                        .child(status.icon())
                        .child(detail::run_status_label(run.status)),
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

/// Whether a scroll handle is close enough to the bottom to count as the tail.
fn at_bottom(scroll: &ScrollHandle) -> bool {
    let max = f32::from(scroll.max_offset().height);
    if max <= 0.0 {
        return true;
    }
    let offset = -f32::from(scroll.offset().y);
    offset >= max - FOLLOW_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> &'static Theme {
        Theme::for_appearance(gpui::WindowAppearance::Dark)
    }

    fn inline_of(source: &str) -> markdown::InlineText {
        match markdown::parse(source).blocks.into_iter().next() {
            Some(Block::Paragraph(content)) => markdown::flatten(&content),
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    fn runs_of(source: &str) -> (Vec<TextRun>, Vec<(Range<usize>, String)>) {
        let theme = theme();
        inline_runs(
            theme,
            &TextStyle::default(),
            &inline_of(source),
            Tone::Prose,
            theme.font.sm,
            FontWeight::NORMAL,
            Hsla::from(theme.colors.text),
        )
    }

    #[test]
    fn inline_runs_apply_strong_emphasis_strike_and_code() {
        let (bold, _) = runs_of("**bold**");
        assert_eq!(bold.len(), 1);
        assert_eq!(bold[0].font.weight, FontWeight::SEMIBOLD);

        let (italic, _) = runs_of("*italic*");
        assert_eq!(italic[0].font.style, FontStyle::Italic);

        let (strike, _) = runs_of("~~gone~~");
        assert!(strike[0].strikethrough.is_some());

        let (code, _) = runs_of("`code`");
        assert_eq!(code[0].font.family.as_ref(), "monospace");
        assert!(code[0].background_color.is_some());
    }

    #[test]
    fn inline_runs_expose_openable_links() {
        let (runs, links) = runs_of("[docs](https://example.com)");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "https://example.com");
        let theme = theme();
        assert_eq!(runs[0].color, Hsla::from(theme.colors.accent));
        assert!(runs[0].underline.is_some());
        assert_eq!(runs.iter().map(|run| run.len).sum::<usize>(), "docs".len());
    }

    #[test]
    fn only_http_and_https_links_open() {
        assert!(is_openable("http://example.com"));
        assert!(is_openable("https://example.com/a?b=c"));
        assert!(!is_openable("file:///etc/passwd"));
        assert!(!is_openable("javascript:alert(1)"));
        assert!(!is_openable("/relative/path"));
    }

    #[test]
    fn headings_follow_the_documented_scale_and_weights() {
        let theme = theme();
        let expected = [
            FontWeight::BOLD,
            FontWeight::BOLD,
            FontWeight::SEMIBOLD,
            FontWeight::MEDIUM,
            FontWeight::SEMIBOLD,
            FontWeight::MEDIUM,
        ];
        for (index, weight) in expected.into_iter().enumerate() {
            let level = index as u8 + 1;
            let (size, actual, _) = heading_style(theme, level);
            assert_eq!(size, theme.markdown_heading_size(level));
            assert_eq!(actual, weight);
        }
        assert_eq!(heading_style(theme, 5).2, Hsla::from(theme.colors.muted));
        assert_eq!(heading_style(theme, 1).2, Hsla::from(theme.colors.text));
    }

    #[test]
    fn feed_item_falls_back_to_raw_text_without_a_parsed_body() {
        let row = FeedRow {
            item: FeedItem::Message {
                role: MessageRole::Assistant,
                text: "raw **markdown** source".to_string(),
            },
            body: None,
        };
        // Constructing the row must not panic; the fallback child carries the
        // raw source so a missing cache entry can never blank the message.
        let _ = feed_item(theme(), &TextStyle::default(), 0, &row);
        assert!(row.body.is_none());
    }
}
