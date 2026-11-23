//! The task detail surface: a pinned header, two tabs and the permission
//! banner (docs/design/gpui-client.md §1, §2, §5 step 06).
//!
//! This is the client's core surface, so it follows three rules:
//!
//! - **Working Memory** — the header (name, status, agent, repo, attempt,
//!   elapsed, session id, usage) never scrolls away; only the tab body scrolls.
//! - **Peak-End** — a finished run ends with its outcome and usage in the
//!   header, and every tab has an explicit empty/error state with Retry.
//! - **Von Restorff** — a blocked run gets the only warning-colored surface
//!   (the permission banner); Cancel (live) and Delete (terminal) are mutually
//!   exclusive by status, so at most one danger-colored action is visible.
//!
//! The aggregation and feed grouping live in [`crate::detail`] (pure and
//! unit-tested); this file only paints them.

use std::rc::Rc;
use std::time::Duration;

use favetto_wire::model::{
    AgentAuthState, AgentAuthStatus, AgentCommand, AgentPlanEntry, AgentPlanStatus, AgentUsage,
    ContentRole, InputReply, MessageRole, RunSummary, SessionConfigKind, SessionConfigOption,
    SessionConfigValue, SessionModes, Task, TaskRun, TaskStatus,
};
use favetto_wire::Timestamp;
use gpui::{
    div, list, prelude::*, px, AnyElement, Context, Div, Entity, FocusHandle, Hsla, IntoElement,
    ListAlignment, ListState, Render, ScrollHandle, ScrollWheelEvent, Stateful, TextStyle, Window,
};
use uuid::Uuid;

use crate::components;
use crate::detail::{
    self, DetailTab, FeedItem, OutputSource, OutputText, PendingRequest, SessionRestore,
    TaskDetail, ToolStatus,
};
use crate::elicitation::ElicitationStage;
use crate::inbox::{self, StatusKind};
use crate::layout::{container_query, LayoutMode};
use crate::markdown::{self, MarkdownDoc};
use crate::state::ClientState;
use crate::tasks;
use crate::text_element::{state_text_element, TextElement};
use crate::text_input::TextTarget;
use crate::theme::Theme;
use crate::views::markdown::{self as markdown_view, Tone};

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
    delete_focus: FocusHandle,
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
    /// The always-visible follow-up composer's focus handle. Tab lands here and
    /// a click focuses it; the composer only takes text keys while focused.
    prompt_focus: FocusHandle,
    /// Whether the focus listeners for [`Self::prompt_focus`] are registered.
    prompt_focus_hooked: bool,
    /// Persisted across renders so live events never jump the reader.
    output_scroll: ScrollHandle,
    /// Virtualized feed state: only the visible window is measured/painted.
    events_list: ListState,
    /// The grouped feed, rebuilt only when its source revision changes.
    events_feed: Rc<Vec<FeedRow>>,
    events_feed_key: Option<(Uuid, u8, u64)>,
    /// Parsed Markdown for the message/reasoning rows, keyed by content and
    /// kept across feed revisions until a row disappears.
    markdown: markdown::MarkdownCache,
    markdown_task: Option<Uuid>,
    /// The content length at the last tail scroll, so auto-follow only reacts to
    /// growth (and never fights a deliberate scroll).
    output_rendered: usize,
    /// Whether the merged output tab shows the plain raw transcript instead of
    /// the structured log.
    raw_output_open: bool,
    raw_output_task: Option<Uuid>,
    raw_output_focus: FocusHandle,
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
            delete_focus: cx.focus_handle().tab_index(49).tab_stop(true),
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
            prompt_focus: cx.focus_handle().tab_index(77).tab_stop(true),
            prompt_focus_hooked: false,
            output_scroll: ScrollHandle::new(),
            events_list,
            events_feed: Rc::new(Vec::new()),
            events_feed_key: None,
            markdown: markdown::MarkdownCache::default(),
            markdown_task: None,
            output_rendered: 0,
            raw_output_open: false,
            raw_output_task: None,
            raw_output_focus: cx.focus_handle().tab_index(69).tab_stop(true),
        };
        view.start_clock(cx);
        view
    }

    /// Register the composer's focus listeners once. Real focus drives the
    /// composer's enabled state: Tab landing on its handle (or a click) starts
    /// routing text to the draft, and leaving it hands the shell its keys back.
    fn hook_prompt_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompt_focus_hooked {
            return;
        }
        self.prompt_focus_hooked = true;
        let focus = self.prompt_focus.clone();
        cx.on_focus(&focus, window, |this, _window, cx| {
            this.state.update(cx, |state, cx| {
                state.focus_detail_prompt();
                cx.notify();
            });
        })
        .detach();
        cx.on_blur(&focus, window, |this, _window, cx| {
            this.state.update(cx, |state, cx| {
                state.blur_detail_prompt();
                cx.notify();
            });
        })
        .detach();
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

    fn follow_toggle(
        &self,
        theme: &Theme,
        follow: bool,
        live: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
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
                .child(if follow {
                    components::pulse_glyph(
                        "◉",
                        theme.colors.accent,
                        theme.font.xs,
                        "detail-follow-pulse",
                        live,
                    )
                } else {
                    div()
                        .flex_shrink_0()
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.muted)
                        .child("○")
                        .into_any_element()
                })
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

    /// Toggle the merged output tab between the structured log and the plain
    /// persisted transcript. The raw text is only rendered on demand, never
    /// duplicated underneath the feed.
    fn output_mode_toggle(
        &self,
        theme: &Theme,
        raw_open: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        components::with_focus(
            div()
                .id("detail-output-mode")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s1))
                .px(px(theme.spacing.s2))
                .py(px(theme.spacing.s1))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(theme.colors.border)
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .cursor_pointer()
                .hover(|style| style.bg(theme.colors.bg))
                .track_focus(&self.raw_output_focus)
                .child(if raw_open { "▤" } else { "≡" })
                .child(if raw_open {
                    "Structured log"
                } else {
                    "Raw output"
                })
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.raw_output_open = !raw_open;
                    cx.notify();
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
                    .when_some(session_row, |row, session| {
                        row.child(div().flex_1().min_w_0().child(session))
                    })
                    .when(data.session_id.is_none(), |row| row.child(div().flex_1()))
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

    /// The session option/mode strip below the header. It renders the daemon's
    /// complete snapshot; clicking a supported selector cycles to the next
    /// advertised value and sends a validated `agents.set_option` / `set_mode`.
    fn session_options_bar(
        &self,
        theme: &Theme,
        snapshot: (String, Vec<SessionConfigOption>, Option<SessionModes>),
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let (_session_id, options, modes) = snapshot;
        let mut pills: Vec<AnyElement> = Vec::new();

        if let Some(modes) = modes.filter(|modes| !modes.available_modes.is_empty()) {
            let current = modes
                .available_modes
                .iter()
                .find(|mode| mode.id == modes.current_mode_id)
                .map(|mode| mode.name.clone())
                .unwrap_or_else(|| modes.current_mode_id.clone());
            if let Some(next) = next_mode(&modes, &modes.current_mode_id) {
                let mode_id = next.id.clone();
                let index = pills.len();
                pills.push(self.session_pill(
                    theme,
                    index,
                    "mode",
                    current,
                    Some(format!("Next: {}", next.name)),
                    cx.listener(move |this, _event, _window, cx| {
                        this.state.update(cx, |state, cx| {
                            state.set_detail_session_mode(mode_id.clone(), cx);
                            cx.notify();
                        });
                    }),
                ));
            } else {
                pills.push(read_only_pill(theme, "mode", current));
            }
        }

        for option in options {
            let label = option
                .category
                .as_deref()
                .unwrap_or(&option.name)
                .to_string();
            match &option.kind {
                SessionConfigKind::Select {
                    current_value,
                    choices,
                } => {
                    let all = choices.all();
                    let current = all
                        .iter()
                        .find(|choice| &choice.value == current_value)
                        .map(|choice| choice.name.clone())
                        .unwrap_or_else(|| current_value.clone());
                    let next = all
                        .iter()
                        .position(|choice| &choice.value == current_value)
                        .map(|index| &all[(index + 1) % all.len()])
                        .filter(|_choice| all.len() > 1);
                    if let Some(next) = next {
                        let option_id = option.id.clone();
                        let value = SessionConfigValue::ValueId {
                            value: next.value.clone(),
                        };
                        let index = pills.len();
                        pills.push(self.session_pill(
                            theme,
                            index,
                            &label,
                            current,
                            Some(format!("Next: {}", next.name)),
                            cx.listener(move |this, _event, _window, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.set_detail_session_option(
                                        option_id.clone(),
                                        value.clone(),
                                        cx,
                                    );
                                    cx.notify();
                                });
                            }),
                        ));
                    } else {
                        pills.push(read_only_pill(theme, &label, current));
                    }
                }
                SessionConfigKind::Boolean { current_value } => {
                    let value = SessionConfigValue::Boolean {
                        value: !current_value,
                    };
                    let option_id = option.id.clone();
                    let index = pills.len();
                    pills.push(self.session_pill(
                        theme,
                        index,
                        &label,
                        if *current_value { "on" } else { "off" }.to_string(),
                        Some(format!(
                            "Click to turn {}",
                            if *current_value { "off" } else { "on" }
                        )),
                        cx.listener(move |this, _event, _window, cx| {
                            this.state.update(cx, |state, cx| {
                                state.set_detail_session_option(
                                    option_id.clone(),
                                    value.clone(),
                                    cx,
                                );
                                cx.notify();
                            });
                        }),
                    ));
                }
                SessionConfigKind::Other => {
                    pills.push(read_only_pill(theme, &label, "unsupported".to_string()));
                }
            }
        }

        if pills.is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(theme.spacing.s1))
                .px(px(theme.spacing.s3))
                .py(px(theme.spacing.s1))
                .border_b_1()
                .border_color(theme.colors.border)
                .bg(theme.colors.surface)
                .child(components::section_label(theme, "SESSION"))
                .children(pills),
        )
    }

    fn session_pill(
        &self,
        theme: &Theme,
        index: usize,
        label: &str,
        value: String,
        hint: Option<String>,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> AnyElement {
        let hint = hint.unwrap_or_default();
        div()
            .id(("session-option", index))
            .flex()
            .items_center()
            .gap(px(theme.spacing.s1))
            .px(px(theme.spacing.s2))
            .py(px(2.0))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.bg)
            .text_size(px(theme.font.xs))
            .text_color(theme.colors.text)
            .cursor_pointer()
            .hover(|style| style.border_color(theme.colors.accent))
            .child(
                div()
                    .text_color(theme.colors.muted)
                    .child(format!("{label}:")),
            )
            .child(div().child(format!("{value} ▾")))
            .when(!hint.is_empty(), |pill| {
                pill.child(
                    div()
                        .text_color(theme.colors.muted)
                        .child(format!("({hint})")),
                )
            })
            .on_click(on_click)
            .into_any_element()
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
        live: bool,
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
            let badge = (candidate == DetailTab::Output && new_events > 0).then(|| {
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
            .child(self.follow_toggle(theme, follow, live, cx))
    }

    /// The pinned ACP sign-in banner. The daemon performs the flow; the client
    /// only selects an advertised agent-handled method (or logout when
    /// advertised). It never displays or accepts credentials.
    fn auth_banner(
        &self,
        theme: &Theme,
        session_id: &str,
        auth: &AgentAuthState,
        cx: &mut Context<Self>,
    ) -> Div {
        let (headline, color) = match auth.status {
            AgentAuthStatus::Required => (
                "Agent needs sign-in before this session can start.",
                theme.colors.warning,
            ),
            AgentAuthStatus::Authenticating => {
                ("Signing in through the agent…", theme.colors.accent)
            }
            AgentAuthStatus::Failed => (
                "Sign-in failed; retry with an advertised method.",
                theme.colors.danger,
            ),
            AgentAuthStatus::NotRequired | AgentAuthStatus::Authenticated => {
                if !auth.logout_supported {
                    return div();
                }
                ("Signed in to the agent.", theme.colors.success)
            }
        };
        let supported: Vec<_> = auth
            .methods
            .iter()
            .filter(|method| method.supported)
            .collect();
        let mut actions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(theme.spacing.s1));
        for (index, method) in supported.iter().enumerate() {
            let session_id = session_id.to_string();
            let method_id = method.id.clone();
            let enabled = auth.status != AgentAuthStatus::Authenticating
                && auth.status != AgentAuthStatus::Authenticated;
            actions = actions.child(components::with_focus(
                div()
                    .id(("detail-auth", index))
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(theme.controls.primary))
                    .px(px(theme.spacing.s3))
                    .rounded(px(theme.radii.r4))
                    .border_1()
                    .border_color(Hsla::from(theme.colors.border))
                    .text_size(px(theme.font.xs))
                    .when(enabled, |button| {
                        button
                            .text_color(theme.colors.text)
                            .cursor_pointer()
                            .hover(|style| style.bg(Hsla::from(theme.colors.accent).opacity(0.16)))
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.authenticate(session_id.clone(), method_id.clone(), cx);
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
            let session_id = session_id.to_string();
            actions = actions.child(components::with_focus(
                div()
                    .id("detail-auth-logout")
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(theme.controls.primary))
                    .px(px(theme.spacing.s3))
                    .rounded(px(theme.radii.r4))
                    .border_1()
                    .border_color(Hsla::from(theme.colors.border))
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
        let terminal_only = supported.is_empty()
            && auth
                .methods
                .iter()
                .any(|method| method.kind == favetto_wire::model::AgentAuthMethodKind::Terminal);
        let status = auth
            .message
            .clone()
            .or_else(|| {
                terminal_only.then(|| {
                    "Terminal sign-in cannot run remotely; configure credentials on the daemon host."
                        .to_string()
                })
            })
            .unwrap_or_else(|| {
                "The daemon completes the agent's own sign-in flow; no credential is sent by this client."
                    .to_string()
            });

        div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s3))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s2))
            .border_b_1()
            .border_color(Hsla::from(color).opacity(0.35))
            .bg(Hsla::from(color).opacity(0.08))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex_col()
                    .gap(px(theme.spacing.s1))
                    .child(
                        div()
                            .text_size(px(theme.font.xs))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(headline),
                    )
                    .child(
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child(status),
                    ),
            )
            .child(actions)
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

    /// The merged output tab: choose the live feed, the durable run summary, or
    /// the plain persisted transcript, and keep the raw transcript one click
    /// away whenever a structured log is available.
    fn merged_output_body(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let restore = self
            .state
            .read(cx)
            .detail()
            .map(|detail| detail.restore.clone());
        let panel = restore.and_then(|restore| self.restore_panel(theme, &restore, cx));
        let body = self.merged_output_content(theme, cx);
        match panel {
            Some(panel) => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(panel)
                .child(div().flex_1().min_h_0().child(body))
                .into_any_element(),
            None => body,
        }
    }

    /// A read-only banner/transcript for the task-scoped agent conversation.
    ///
    /// The restored text is only painted when no structured feed is available;
    /// otherwise the replayed feed below is the conversation.
    fn restore_panel(
        &self,
        theme: &Theme,
        restore: &SessionRestore,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        let feed_present = self
            .state
            .read(cx)
            .detail()
            .is_some_and(|detail| !detail.events.is_empty());
        let (headline, detail, color) = match restore {
            SessionRestore::Checking => (
                "Restoring conversation…".to_string(),
                "Looking up the agent session associated with this run.".to_string(),
                theme.colors.muted,
            ),
            SessionRestore::Attaching {
                session_id,
                agent,
                live,
            } => (
                "Restoring conversation…".to_string(),
                format!(
                    "Attaching to session {session_id} ({agent}){}.",
                    if *live { " · live" } else { "" }
                ),
                theme.colors.warning,
            ),
            SessionRestore::Restored(restored) => (
                "Restored conversation".to_string(),
                format!(
                    "{} · session {} · {}",
                    restored.agent,
                    restored.session_id,
                    restored.state_label(),
                ),
                theme.colors.success,
            ),
            SessionRestore::Unavailable(reason) => (
                "No agent conversation to restore".to_string(),
                reason.clone(),
                theme.colors.muted,
            ),
            SessionRestore::Failed(reason) => (
                "The agent conversation could not be restored".to_string(),
                reason.clone(),
                theme.colors.danger,
            ),
        };

        let mut panel = div()
            .id("detail-restore")
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s2))
            .border_b_1()
            .border_color(theme.colors.border)
            .bg(Hsla::from(theme.colors.border).opacity(0.12))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s1))
                    .text_size(px(theme.font.xs))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(color)
                    .child("●")
                    .child(headline),
            )
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(detail),
            );

        if let SessionRestore::Restored(restored) = restore {
            if restored.has_transcript() && !feed_present {
                panel = panel.child(
                    div()
                        .id("detail-restored-transcript")
                        .mt(px(theme.spacing.s1))
                        .max_h(px(240.0))
                        .overflow_y_scroll()
                        .font_family("monospace")
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.text)
                        .whitespace_normal()
                        .child(restored.transcript.clone()),
                );
            } else if feed_present {
                panel = panel.child(
                    div()
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.muted)
                        .child("The replayed conversation is shown in the structured feed below."),
                );
            }
        }

        Some(panel)
    }

    fn merged_output_content(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let body = self.output_view(theme, cx);
        let mut column = div().flex_1().min_h_0().flex().flex_col();
        // The composer is part of the Output tab itself, so it stays under every
        // body state (loading, error, empty, live feed, raw transcript) instead
        // of being replaced by it.
        column = column.child(div().flex_1().min_h_0().flex().flex_col().child(body));
        if let Some(composer) = self.prompt_composer(theme, cx) {
            column = column.child(composer);
        }
        column.into_any_element()
    }

    /// The Output tab body without the always-visible composer.
    fn output_view(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (
            task_id,
            status,
            terminal,
            live_present,
            output,
            task_loading,
            task_error,
            events_loading,
            events_loaded,
            events_error,
            session,
            ignored,
            events_revision,
            plan,
            summary,
        ) = {
            let state = self.state.read(cx);
            let detail = state.detail();
            let task = detail.and_then(|detail| detail.task.as_ref());
            (
                detail.map(|detail| detail.task_id),
                detail
                    .and_then(TaskDetail::status)
                    .unwrap_or(TaskStatus::Pending),
                detail.is_some_and(TaskDetail::is_terminal),
                detail.is_some_and(|detail| !detail.events.is_empty() || !detail.plan().is_empty()),
                detail.and_then(TaskDetail::output),
                detail.is_some_and(|detail| detail.task_loading && detail.task.is_none()),
                detail.and_then(|detail| detail.task_error.clone()),
                detail.is_some_and(|detail| detail.events_loading),
                detail.is_some_and(|detail| detail.events_loaded),
                detail.and_then(|detail| detail.events_error.clone()),
                detail.and_then(|detail| detail.events_session.clone()),
                detail.map(|detail| detail.events_ignored).unwrap_or(0),
                detail.map(|detail| detail.events_revision).unwrap_or(0),
                state.detail_plan(),
                task.and_then(|task| task.output.as_ref())
                    .and_then(detail::extract_run_summary),
            )
        };
        let Some(task_id) = task_id else {
            return div().into_any_element();
        };
        let source = detail::output_source(status, live_present, summary.is_some());
        let raw_available = output.is_some();
        if self.raw_output_open && !raw_available {
            // A retried attempt starts without a persisted blob; don't reopen
            // the raw view automatically when the next run finishes.
            self.raw_output_open = false;
        }
        let raw_open = self.raw_output_open && source != OutputSource::Plain && raw_available;

        if raw_open {
            let mut column = div().flex().flex_col().flex_1().min_h_0();
            column = column.child(
                div()
                    .flex()
                    .justify_end()
                    .px(px(theme.spacing.s3))
                    .pt(px(theme.spacing.s2))
                    .child(self.output_mode_toggle(theme, true, cx)),
            );
            if let Some(message) = task_error.as_deref() {
                column = column.child(
                    div()
                        .px(px(theme.spacing.s3))
                        .pt(px(theme.spacing.s2))
                        .child(self.error_banner(theme, message, cx)),
                );
            }
            if let Some(output) = output {
                column =
                    column.child(self.output_body(theme, Some(output), false, None, terminal, cx));
            }
            return column.into_any_element();
        }

        if source == OutputSource::Plain {
            if output.is_none() {
                if task_loading || (events_loading && !events_loaded) {
                    return self.loading(theme, "OUTPUT").into_any_element();
                }
                if let Some(message) = events_error {
                    return self
                        .error_body(
                            theme,
                            ErrorBody {
                                label: "OUTPUT",
                                title: "The output log could not load",
                                message: &message,
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
                        )
                        .into_any_element();
                }
                if let Some(message) = task_error {
                    return self
                        .error_body(
                            theme,
                            ErrorBody {
                                label: "OUTPUT",
                                title: "The run output could not load",
                                message: &message,
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
            }
            return self
                .output_body(theme, output, false, task_error, terminal, cx)
                .into_any_element();
        }

        let revision = match source {
            OutputSource::Live => events_revision,
            OutputSource::Summary => summary.as_ref().map(summary_revision).unwrap_or(0),
            OutputSource::Plain => 0,
        };
        let feed = self.feed_for(task_id, source, revision, summary.as_ref(), cx);
        let events_error = (source == OutputSource::Live)
            .then_some(events_error)
            .flatten();
        self.events_body(
            theme,
            EventsBody {
                feed,
                loading: events_loading,
                loaded: events_loaded,
                error: events_error,
                task_error,
                session,
                ignored,
                plan,
                raw_available,
            },
            cx,
        )
        .into_any_element()
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
                    "The daemon has no output blob for this run. The Runs tab keeps the attempt record."
                } else {
                    "The daemon persists output when the run produces it. Live events appear here while it runs."
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
        source: OutputSource,
        revision: u64,
        summary: Option<&RunSummary>,
        cx: &mut Context<Self>,
    ) -> Rc<Vec<FeedRow>> {
        let source_key = match source {
            OutputSource::Live => 0,
            OutputSource::Summary => 1,
            OutputSource::Plain => 2,
        };
        let key = (task_id, source_key, revision);
        if self.events_feed_key != Some(key) {
            let feed = match source {
                OutputSource::Live => {
                    let state = self.state.read(cx);
                    state
                        .detail()
                        .filter(|detail| detail.task_id == task_id)
                        .map(TaskDetail::feed)
                        .unwrap_or_default()
                }
                OutputSource::Summary => summary.map(detail::summary_feed).unwrap_or_default(),
                OutputSource::Plain => Vec::new(),
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
            task_error,
            session,
            ignored,
            plan,
            raw_available,
        } = data;
        let has_plan = !plan.is_empty();
        if loading && feed.is_empty() && !loaded && !has_plan {
            return self.loading(theme, "OUTPUT");
        }
        if feed.is_empty() && events_error.is_some() && !has_plan {
            return self.error_body(
                theme,
                ErrorBody {
                    label: "OUTPUT",
                    title: "The output log could not load",
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
        if feed.is_empty() && !has_plan {
            if ignored > 0 {
                return self.empty_body(
                    theme,
                    "≋",
                    "OUTPUT",
                    "Output from a newer daemon",
                    &format!(
                        "{ignored} event{} used a kind this client does not render yet. Upgrade the client to see them.",
                        if ignored == 1 { "" } else { "s" }
                    ),
                );
            }
            let (title, body) = if session.is_some() {
                (
                    "No structured output yet",
                    "The session is attached but has not reported a structured event yet. Live events land here as they arrive.",
                )
            } else {
                (
                    "No structured log",
                    "This run does not report structured events (a PTY-backed agent). Its persisted output appears here when available.",
                )
            };
            return self.empty_body(theme, "≋", "OUTPUT", title, body);
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

        let plan_card = has_plan.then(|| self.plan_card(theme, &plan));
        let raw_toggle = raw_available.then(|| {
            div()
                .flex()
                .justify_end()
                .px(px(theme.spacing.s3))
                .pt(px(theme.spacing.s2))
                .child(self.output_mode_toggle(theme, false, cx))
        });

        let mut container = div().relative().flex_1().min_h_0().flex().flex_col();
        if let Some(plan) = plan_card {
            container = container.child(plan);
        }
        if let Some(message) = task_error {
            container = container.child(
                div()
                    .px(px(theme.spacing.s3))
                    .pt(px(theme.spacing.s2))
                    .child(self.error_banner(theme, &message, cx)),
            );
        }
        if let Some(message) = events_error {
            container = container.child(
                div()
                    .px(px(theme.spacing.s3))
                    .pt(px(theme.spacing.s2))
                    .child(self.error_banner(theme, &message, cx)),
            );
        }
        if let Some(toggle) = raw_toggle {
            container = container.child(toggle);
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

    /// A compact plan card above the output feed.
    fn plan_card(&self, theme: &Theme, plan: &[AgentPlanEntry]) -> Div {
        const MAX_PLAN_ROWS: usize = 12;
        let completed = plan
            .iter()
            .filter(|entry| entry.status == AgentPlanStatus::Completed)
            .count();
        let mut card = div()
            .mx(px(theme.spacing.s3))
            .mt(px(theme.spacing.s2))
            .p(px(theme.spacing.s2))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.bg)
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .text_size(px(theme.font.xs))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.colors.accent)
                    .child("PLAN")
                    .child(
                        div()
                            .text_color(theme.colors.muted)
                            .child(format!("· {completed}/{}", plan.len())),
                    ),
            );
        for entry in plan.iter().take(MAX_PLAN_ROWS) {
            let (glyph, color) = match entry.status {
                AgentPlanStatus::Completed => ("[x]", theme.colors.success),
                AgentPlanStatus::InProgress => ("[>]", theme.colors.accent),
                AgentPlanStatus::Pending => ("[ ]", theme.colors.text),
                AgentPlanStatus::Unknown => ("[?]", theme.colors.muted),
            };
            let content = detail::strip_ansi(&entry.content).replace(['\n', '\r'], " ");
            let priority = match entry.priority {
                favetto_wire::model::AgentPlanPriority::High => " (!)",
                _ => "",
            };
            card = card.child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(theme.spacing.s1))
                    .text_size(px(theme.font.sm))
                    .child(div().text_color(color).child(glyph))
                    .child(
                        div()
                            .min_w_0()
                            .text_color(theme.colors.text)
                            .child(format!("{content}{priority}")),
                    ),
            );
        }
        if plan.len() > MAX_PLAN_ROWS {
            card = card.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(format!("… {} more", plan.len() - MAX_PLAN_ROWS)),
            );
        }
        card
    }

    /// The always-visible follow-up prompt composer, including slash-command
    /// completion. Returns `None` when no run detail is open (there is nothing
    /// to reply to).
    fn prompt_composer(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<Div> {
        let (commands, prompt, selected, sending, error, enabled, focused, disabled_reason) = {
            let state = self.state.read(cx);
            let detail = state.detail()?;
            let enabled = state.detail_prompt_enabled();
            (
                state.detail_commands(),
                detail.prompt.value().to_string(),
                detail.prompt_selected,
                detail.prompt_sending,
                detail.prompt_error.clone(),
                enabled,
                detail.prompt_focused && enabled,
                state.detail_prompt_disabled_reason(),
            )
        };
        let matches: Vec<(usize, &AgentCommand)> = if enabled {
            detail::matching_commands(&commands, &prompt)
                .into_iter()
                .take(5)
                .collect()
        } else {
            Vec::new()
        };
        let exact = prompt
            .strip_prefix('/')
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|name| commands.iter().find(|command| command.name == name));
        let hint = exact.and_then(|command| command.input_hint.as_deref());
        let placeholder = disabled_reason.unwrap_or("Type a message, or / for commands");

        let mut composer = div()
            .m(px(theme.spacing.s3))
            .mt(px(theme.spacing.s2))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .id("detail-prompt")
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s1))
                    .px(px(theme.spacing.s2))
                    .py(px(theme.spacing.s1))
                    .rounded(px(theme.radii.r4))
                    .border_1()
                    .border_color(if focused {
                        theme.colors.accent
                    } else {
                        theme.colors.border
                    })
                    .bg(theme.colors.bg)
                    .text_size(px(theme.font.sm))
                    .when(enabled, |input| {
                        input
                            .track_focus(&self.prompt_focus)
                            .cursor_text()
                            .on_click(cx.listener(|this, _event, window, cx| {
                                window.focus(&this.prompt_focus);
                                this.state.update(cx, |state, cx| {
                                    state.focus_detail_prompt();
                                    cx.notify();
                                });
                            }))
                    })
                    .when(!enabled, |input| input.opacity(0.55).cursor_default())
                    .child(
                        div()
                            .text_color(if enabled {
                                theme.colors.accent
                            } else {
                                theme.colors.muted
                            })
                            .child(if sending { "…" } else { ">" }),
                    )
                    .child(
                        div().flex_1().min_w_0().text_size(px(theme.font.sm)).child(
                            TextElement::new(
                                self.state.clone(),
                                TextTarget::DetailPrompt,
                                self.prompt_focus.clone(),
                                theme,
                            )
                            .multiline(true)
                            .placeholder(placeholder),
                        ),
                    )
                    .child(
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child(if enabled {
                                "↵ send · tab complete · esc back"
                            } else {
                                "read-only"
                            }),
                    ),
            );
        for (position, (index, command)) in matches.into_iter().enumerate() {
            let active = selected.unwrap_or(0) == position;
            let row = div()
                .id(("detail-command", index))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .px(px(theme.spacing.s2))
                .py(px(2.0))
                .rounded(px(theme.radii.r4))
                .cursor_pointer()
                .when(active, |row| row.bg(theme.colors.bg))
                .text_size(px(theme.font.sm))
                .child(
                    div()
                        .text_color(if active {
                            theme.colors.accent
                        } else {
                            theme.colors.text
                        })
                        .child(format!("/{}", command.name)),
                )
                .child(
                    div()
                        .min_w_0()
                        .text_color(theme.colors.muted)
                        .child(detail::strip_ansi(&command.description)),
                );
            composer =
                composer.child(row.on_click(cx.listener(move |this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.detail_prompt_choose_command(index);
                        cx.notify();
                    });
                })));
        }
        if let Some(hint) = hint {
            composer = composer.child(
                div()
                    .px(px(theme.spacing.s2))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(detail::strip_ansi(hint)),
            );
        }
        if let Some(error) = error {
            composer = composer.child(
                div()
                    .px(px(theme.spacing.s2))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.danger)
                    .child(error.to_string()),
            );
        }
        Some(composer)
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

        let now = Timestamp::now();
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

/// Everything the merged output feed needs, extracted from the state in one go.
struct EventsBody {
    feed: Rc<Vec<FeedRow>>,
    loading: bool,
    loaded: bool,
    error: Option<String>,
    task_error: Option<String>,
    session: Option<String>,
    ignored: usize,
    plan: Vec<AgentPlanEntry>,
    raw_available: bool,
}

/// One rendered feed row plus the parsed Markdown for prose/reasoning items.
struct FeedRow {
    item: FeedItem,
    body: Option<Rc<MarkdownDoc>>,
}

/// A stable fingerprint for a durable summary, so the grouped feed is rebuilt
/// only when the persisted log actually changes.
fn summary_revision(summary: &RunSummary) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(summary)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
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
        self.hook_prompt_focus(window, cx);
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
                    "Open a run in the list to see its status, agent and attempt here. Its output and attempt history stay one tab away.",
                    "Open a row to inspect it",
                ));
        };

        if self.raw_output_task != Some(task.id) {
            self.raw_output_task = Some(task.id);
            self.raw_output_open = false;
        }

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
        let session_options = self.state.read(cx).detail_session_options();
        let options_bar =
            session_options.and_then(|snapshot| self.session_options_bar(theme, snapshot, cx));
        let auth = {
            let state = self.state.read(cx);
            state
                .detail_auth()
                .map(|(session, auth)| (session.id.clone(), auth.clone()))
        };
        let auth_banner = auth
            .as_ref()
            .map(|(session_id, auth)| self.auth_banner(theme, session_id, auth, cx));
        let banner = pending.as_ref().map(|request| {
            let in_flight = self
                .state
                .read(cx)
                .detail()
                .is_some_and(|detail| detail.reply_in_flight(request.request_id.as_deref()));
            self.banner(theme, request, in_flight, cx)
        });
        let elicitation = self.state.read(cx).detail_elicitation().cloned();
        let elicitation_modal = elicitation
            .as_ref()
            .map(|form| self.elicitation_modal(theme, form, cx));
        let tabs = self.tabs(
            theme,
            tab,
            new_events,
            follow,
            !tasks::is_terminal(task.status),
            cx,
        );
        let current_attempt = task.attempt;

        let body = match tab {
            DetailTab::Output => self.merged_output_body(theme, cx),
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
            .when_some(options_bar, |detail, bar| detail.child(bar))
            .when_some(auth_banner, |detail, banner| detail.child(banner))
            .when_some(banner, |detail, banner| detail.child(banner))
            .child(tabs)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .when_some(elicitation_modal, |detail, modal| detail.child(modal))
    }
}

impl DetailView {
    /// The accessible ACP form modal: agent identity, labelled controls, review
    /// and explicit submit/decline/cancel actions.
    fn elicitation_modal(
        &self,
        theme: &Theme,
        form: &crate::elicitation::ElicitationFormState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut panel = div()
            .w(px(620.0))
            .max_h(px(620.0))
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s3))
            .rounded(px(theme.radii.r8))
            .border_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface);
        panel = panel
            .child(components::section_label(theme, "AGENT FORM"))
            .child(
                div()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(format!("requested by {}", form.request.agent)),
            )
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(form.request.message.clone()),
            );
        if let Some(title) = &form.request.form.title {
            panel = panel.child(
                div()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(title.clone()),
            );
        }
        if let Some(description) = &form.request.form.description {
            panel = panel.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(description.clone()),
            );
        }

        match form.stage {
            ElicitationStage::Editing => {
                for (index, field) in form.fields().iter().enumerate() {
                    let selected = index == form.current;
                    let mut row = div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .p(px(theme.spacing.s1))
                        .rounded(px(theme.radii.r4))
                        .when(selected, |row| {
                            row.border_1()
                                .border_color(theme.colors.accent)
                                .bg(Hsla::from(theme.colors.accent).opacity(0.08))
                        });
                    row = row.child(
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(if selected {
                                theme.colors.accent
                            } else {
                                theme.colors.muted
                            })
                            .child(format!(
                                "{}{}",
                                crate::elicitation::ElicitationFormState::field_label(field),
                                if field.required { " *" } else { "" }
                            )),
                    );
                    if let Some(description) = &field.description {
                        row = row.child(
                            div()
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.muted)
                                .child(description.clone()),
                        );
                    }
                    let editable = matches!(
                        form.drafts.get(index),
                        Some(crate::elicitation::ElicitationDraft::Text(_))
                    );
                    row = row.child(if editable {
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(theme.font.sm))
                            .child(
                                state_text_element(
                                    &self.state,
                                    TextTarget::Elicitation(index),
                                    theme,
                                    cx,
                                )
                                .placeholder("Type a value…"),
                            )
                            .into_any_element()
                    } else {
                        div()
                            .text_size(px(theme.font.sm))
                            .text_color(theme.colors.text)
                            .child(form.display_value(index))
                            .into_any_element()
                    });
                    panel = panel.child(row);
                }
            }
            ElicitationStage::Review => {
                panel = panel.child(
                    div()
                        .text_size(px(theme.font.sm))
                        .text_color(theme.colors.text)
                        .child("Review before sending"),
                );
                for (index, field) in form.fields().iter().enumerate() {
                    let label =
                        crate::elicitation::ElicitationFormState::field_label(field).to_string();
                    let value = form.display_value(index);
                    panel = panel.child(
                        div()
                            .flex()
                            .justify_between()
                            .gap(px(theme.spacing.s2))
                            .text_size(px(theme.font.xs))
                            .child(div().text_color(theme.colors.muted).child(label))
                            .child(div().text_color(theme.colors.text).child(value)),
                    );
                }
            }
        }
        if let Some(error) = &form.error {
            panel = panel.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.danger)
                    .child(error.clone()),
            );
        }

        let mut actions = div().flex().items_center().gap(px(theme.spacing.s2));
        if form.stage == ElicitationStage::Review {
            actions = actions.child(self.action_button(
                theme,
                "elicitation-submit",
                "Submit",
                false,
                &self.reply_once_focus,
                cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.elicitation_confirm(cx);
                        cx.notify();
                    });
                }),
            ));
            actions = actions.child(self.action_button(
                theme,
                "elicitation-decline",
                "Decline",
                true,
                &self.reply_reject_focus,
                cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.elicitation_decline(cx);
                        cx.notify();
                    });
                }),
            ));
            actions = actions.child(self.action_button(
                theme,
                "elicitation-edit",
                "Edit",
                false,
                &self.reply_always_focus,
                cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.elicitation_edit();
                        cx.notify();
                    });
                }),
            ));
        } else {
            actions = actions.child(self.action_button(
                theme,
                "elicitation-review",
                "Review",
                false,
                &self.reply_once_focus,
                cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.elicitation_confirm(cx);
                        cx.notify();
                    });
                }),
            ));
        }
        actions = actions.child(self.action_button(
            theme,
            "elicitation-cancel",
            "Cancel",
            true,
            &self.cancel_focus,
            cx.listener(|this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.elicitation_cancel(cx);
                    cx.notify();
                });
            }),
        ));
        panel = panel.child(
            div()
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .child(
                    "Tab/arrows move · Enter validates/reviews · Esc cancels. Values are sent only \
                     to the agent.",
                ),
        );
        panel = panel.child(actions);

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(Hsla::from(theme.colors.bg).opacity(0.86))
            .child(panel)
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

/// The next advertised mode after `current`, wrapping once. `None` when there is
/// nothing else to switch to.
fn next_mode<'a>(
    modes: &'a SessionModes,
    current: &str,
) -> Option<&'a favetto_wire::model::SessionMode> {
    if modes.available_modes.len() < 2 {
        return None;
    }
    let index = modes
        .available_modes
        .iter()
        .position(|mode| mode.id == current)?;
    modes
        .available_modes
        .get((index + 1) % modes.available_modes.len())
}

fn read_only_pill(theme: &Theme, label: &str, value: String) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(theme.spacing.s1))
        .px(px(theme.spacing.s2))
        .py(px(2.0))
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(theme.colors.border)
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child(format!("{label}:"))
        .child(value)
        .into_any_element()
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
                Some(doc) => markdown_view::markdown_body(
                    theme,
                    base,
                    doc,
                    Tone::Prose,
                    index as u64 * markdown_view::MARKDOWN_ROW_STRIDE,
                ),
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
                Some(doc) => markdown_view::markdown_body(
                    theme,
                    base,
                    doc,
                    Tone::Reasoning,
                    index as u64 * markdown_view::MARKDOWN_ROW_STRIDE,
                ),
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
            id,
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
                        .child(components::animated_icon(
                            status.icon(),
                            *status == ToolStatus::Running,
                            color,
                            theme.font.xs,
                            components::motion_id("tool-spinner", id),
                        ))
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
        FeedItem::Elicitation { request, resolved } => div()
            .flex()
            .items_start()
            .gap(px(theme.spacing.s1))
            .px(px(theme.spacing.s2))
            .py(px(theme.spacing.s1))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(Hsla::from(theme.colors.accent).opacity(0.4))
            .bg(Hsla::from(theme.colors.accent).opacity(0.08))
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.accent)
                    .child(if *resolved { "✓" } else { "▤" }),
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
                        "{} · {} · {}",
                        request.agent,
                        if *resolved {
                            "answered"
                        } else {
                            "form requested"
                        },
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

fn run_row(theme: &Theme, run: &TaskRun, current_attempt: u32, now: Timestamp) -> Div {
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

    #[test]
    fn next_mode_wraps_and_rejects_an_unadvertised_current() {
        use favetto_wire::model::{SessionMode, SessionModes};

        let modes = SessionModes {
            current_mode_id: "build".to_string(),
            available_modes: vec![
                SessionMode {
                    id: "build".to_string(),
                    name: "Build".to_string(),
                    description: None,
                },
                SessionMode {
                    id: "plan".to_string(),
                    name: "Plan".to_string(),
                    description: None,
                },
            ],
        };
        assert_eq!(
            next_mode(&modes, "build").map(|mode| mode.id.as_str()),
            Some("plan")
        );
        assert_eq!(
            next_mode(&modes, "plan").map(|mode| mode.id.as_str()),
            Some("build")
        );
        assert!(next_mode(&modes, "stale").is_none());

        let one = SessionModes {
            current_mode_id: "build".to_string(),
            available_modes: vec![modes.available_modes[0].clone()],
        };
        assert!(next_mode(&one, "build").is_none());
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
