//! The free-standing Agent surface: a task-free conversation in the active
//! repository's main checkout.
//!
//! The structured conversation itself is [`crate::views::conversation`], shared
//! with the task detail. This view owns only the task-free header: the agent
//! picker, the optional pre-start model override, the frozen cwd, the session
//! lifecycle and the New/Interrupt/Close controls.

use std::path::PathBuf;

use gpui::{
    div, prelude::*, px, AnyElement, Context, Div, Entity, FocusHandle, Hsla, IntoElement, Render,
    Stateful, Window,
};

use crate::components;
use crate::detail;
use crate::layout::{container_query, LayoutMode};
use crate::routes::Route;
use crate::state::ClientState;
use crate::text_element::state_text_element;
use crate::text_input::TextTarget;
use crate::theme::Theme;
use crate::views::conversation::ConversationPanel;

/// The Agent surface view.
pub struct AgentView {
    state: Entity<ClientState>,
    conversation: Entity<ConversationPanel>,
    start_focus: FocusHandle,
    new_focus: FocusHandle,
    interrupt_focus: FocusHandle,
    close_focus: FocusHandle,
    copy_focus: FocusHandle,
}

impl AgentView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        let conversation = cx.new(|cx| ConversationPanel::new(state.clone(), cx));
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self {
            state,
            conversation,
            start_focus: cx.focus_handle().tab_index(40).tab_stop(true),
            new_focus: cx.focus_handle().tab_index(41).tab_stop(true),
            interrupt_focus: cx.focus_handle().tab_index(42).tab_stop(true),
            close_focus: cx.focus_handle().tab_index(43).tab_stop(true),
            copy_focus: cx.focus_handle().tab_index(44).tab_stop(true),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn button(
        &self,
        theme: &Theme,
        id: &'static str,
        label: &'static str,
        danger: bool,
        enabled: bool,
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
                .when(enabled, |button| {
                    button
                        .text_color(color)
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.colors.bg))
                        .track_focus(focus)
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

    fn header(
        &self,
        theme: &Theme,
        data: HeaderData<'_>,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let (status, color) = if data.starting {
            ("starting".to_string(), theme.colors.accent)
        } else if let Some(exit) = data.exit {
            let code = exit
                .code
                .map(|code| format!(" · exit {code}"))
                .unwrap_or_default();
            (format!("ended{code}"), theme.colors.muted)
        } else if data.session.is_some_and(|session| session.running) {
            ("running".to_string(), theme.colors.accent)
        } else if data.session.is_some() {
            ("ended".to_string(), theme.colors.muted)
        } else {
            ("idle".to_string(), theme.colors.muted)
        };
        let usage = data
            .session
            .and_then(|session| session.usage.as_ref())
            .and_then(|usage| detail::usage_summary(Some(usage)));
        let live = data.session.is_some_and(|session| session.running) && data.exit.is_none();
        let busy = data.starting || data.closing;

        let interrupt = live.then(|| {
            self.button(
                theme,
                "agent-interrupt",
                "Interrupt turn",
                false,
                !busy,
                &self.interrupt_focus,
                cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.interrupt_detail_turn(cx);
                        cx.notify();
                    });
                }),
            )
        });
        let close = data.session_id.map(|_| {
            self.button(
                theme,
                "agent-close",
                if data.closing { "Closing…" } else { "Close" },
                true,
                !busy,
                &self.close_focus,
                cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.close_standalone_agent(cx);
                        cx.notify();
                    });
                }),
            )
        });
        let new = self.button(
            theme,
            "agent-new",
            if data.session_id.is_some() {
                "New session"
            } else {
                "Start session"
            },
            false,
            !busy,
            if data.session_id.is_some() {
                &self.new_focus
            } else {
                &self.start_focus
            },
            cx.listener(|this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.start_standalone_agent(cx);
                    cx.notify();
                });
            }),
        );

        let session_row = data.session_id.map(|session_id| {
            let value = session_id.to_string();
            let copy = components::with_focus(
                div()
                    .id("agent-copy-session")
                    .px(px(theme.spacing.s1))
                    .rounded(px(theme.radii.r4))
                    .text_color(theme.colors.muted)
                    .cursor_pointer()
                    .track_focus(&self.copy_focus)
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

        let cwd = data
            .cwd
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "select a repository".to_string());

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
                            .child("✦")
                            .child(status),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child("Agent"),
                    )
                    .when(narrow, |row| {
                        row.child(
                            div()
                                .id("agent-back")
                                .flex_shrink_0()
                                .px(px(theme.spacing.s2))
                                .py(px(2.0))
                                .rounded(px(theme.radii.r4))
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.text)
                                .cursor_pointer()
                                .child("←")
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.state.update(cx, |state, cx| {
                                        state.navigate(Route::Inbox);
                                        cx.notify();
                                    });
                                })),
                        )
                    }),
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
                    .child(meta(theme, "repo", data.repo.unwrap_or("global")))
                    .child(meta(theme, "agent", data.agent.unwrap_or("—")))
                    .child(meta(theme, "cwd", &cwd))
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
                    .when_some(close, |row, close| row.child(close))
                    .child(new),
            )
            .when_some(data.start_error, |header, error| {
                header.child(
                    div()
                        .rounded(px(theme.radii.r4))
                        .border_1()
                        .border_color(Hsla::from(theme.colors.danger).opacity(0.45))
                        .bg(Hsla::from(theme.colors.danger).opacity(0.10))
                        .px(px(theme.spacing.s2))
                        .py(px(theme.spacing.s1))
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.text)
                        .child(error.to_string()),
                )
            })
            .when_some(data.close_error, |header, error| {
                header.child(
                    div()
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.danger)
                        .child(error.to_string()),
                )
            })
    }

    fn start_form(&self, theme: &Theme, data: StartFormData<'_>, cx: &mut Context<Self>) -> Div {
        let mut chips = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(theme.spacing.s1));
        if data.agents.is_empty() {
            chips = chips.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child("No available agents are configured on the daemon."),
            );
        }
        for (index, agent) in data.agents.into_iter().enumerate() {
            let selected = data.chosen == Some(agent.name.as_str());
            let name = agent.name.clone();
            chips = chips.child(components::with_focus(
                div()
                    .id(("agent-choice", index))
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s1))
                    .px(px(theme.spacing.s2))
                    .py(px(3.0))
                    .rounded(px(theme.radii.r12))
                    .border_1()
                    .border_color(if selected {
                        theme.colors.accent
                    } else {
                        theme.colors.border
                    })
                    .bg(if selected {
                        Hsla::from(theme.colors.accent).opacity(0.16)
                    } else {
                        Hsla::from(theme.colors.bg)
                    })
                    .text_size(px(theme.font.xs))
                    .text_color(if selected {
                        theme.colors.accent
                    } else {
                        theme.colors.text
                    })
                    .cursor_pointer()
                    .child(agent.display_name.clone())
                    .when(agent.default, |chip| chip.child("· default"))
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.state.update(cx, |state, cx| {
                            state.select_standalone_agent(&name);
                            cx.notify();
                        });
                    })),
                theme,
            ));
        }

        div()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s2))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s3))
            .border_b_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
            .child(components::section_label(theme, "START A SESSION"))
            .child(chips)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child("model"),
                    )
                    .child(
                        div()
                            .id("agent-model")
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s1))
                            .px(px(theme.spacing.s2))
                            .py(px(theme.spacing.s1))
                            .rounded(px(theme.radii.r4))
                            .border_1()
                            .border_color(if data.model_focused {
                                theme.colors.accent
                            } else {
                                theme.colors.border
                            })
                            .bg(theme.colors.bg)
                            .text_size(px(theme.font.sm))
                            .cursor_text()
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.focus_agent_model();
                                    cx.notify();
                                });
                            }))
                            .child(
                                div().flex_1().min_w_0().text_size(px(theme.font.sm)).child(
                                    state_text_element(
                                        &self.state,
                                        TextTarget::AgentModel,
                                        theme,
                                        cx,
                                    )
                                    .placeholder("Agent default, or provider/model"),
                                ),
                            ),
                    ),
            )
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(format!(
                        "Runs in {} (the active repository's main checkout).",
                        data.cwd
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "the active repository".to_string())
                    )),
            )
            .when_some(data.error, |form, error| {
                form.child(
                    div()
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.danger)
                        .child(error.to_string()),
                )
            })
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child("↵ starts · n starts a fresh session · model/thinking/mode are in the session bar"),
            )
    }
}

/// The header's identity cluster.
struct HeaderData<'a> {
    repo: Option<&'a str>,
    agent: Option<&'a str>,
    cwd: Option<&'a PathBuf>,
    session_id: Option<&'a str>,
    session: Option<&'a favetto_wire::model::AgentSessionInfo>,
    exit: Option<&'a favetto_wire::rpc::AgentExitPush>,
    starting: bool,
    closing: bool,
    start_error: Option<&'a str>,
    close_error: Option<&'a str>,
}

/// The pre-start form's data.
struct StartFormData<'a> {
    agents: Vec<crate::catalog::AgentChoice>,
    chosen: Option<&'a str>,
    cwd: Option<&'a PathBuf>,
    model_focused: bool,
    error: Option<&'a str>,
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

impl Render for AgentView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.state.read(cx).theme(window.appearance());
        let narrow = container_query(window.viewport_size().width) == LayoutMode::Narrow;
        let repo_path = self.state.read(cx).active_repo_path();
        let repo = self.state.read(cx).active_repo().map(str::to_string);
        let (
            session_id,
            session,
            exit,
            starting,
            start_error,
            closing,
            close_error,
            cwd,
            agent_repo,
            chosen,
            model_focused,
            agents,
        ) = {
            let state = self.state.read(cx);
            let agent = state.standalone_agent();
            (
                agent.session_id.clone(),
                state.active_agent_session().cloned(),
                agent.exit.clone(),
                agent.starting,
                agent.start_error.clone(),
                agent.closing,
                agent.close_error.clone(),
                agent.cwd.clone(),
                agent.repo.clone(),
                agent.chosen_agent.clone(),
                agent.model_focused,
                state.available_agent_choices(),
            )
        };
        let _ = self.state.read(cx).agent_model_input();
        let has_session = session_id.is_some();
        let header_repo = if has_session {
            agent_repo.as_deref().or(repo.as_deref())
        } else {
            repo.as_deref()
        };

        if repo_path.is_none() && !has_session {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(theme.colors.surface)
                .child(self.header(
                    theme,
                    HeaderData {
                        repo: header_repo,
                        agent: chosen.as_deref(),
                        cwd: cwd.as_ref(),
                        session_id: session_id.as_deref(),
                        session: session.as_ref(),
                        exit: exit.as_ref(),
                        starting,
                        closing,
                        start_error: start_error.as_deref(),
                        close_error: close_error.as_deref(),
                    },
                    narrow,
                    cx,
                ))
                .child(components::empty_state(
                    theme,
                    "✦",
                    "AGENT",
                    "Select a repository",
                    "The Agent panel starts a free-standing ACP session in the active repository's main checkout. Choose a repository in the top bar first; the daemon cwd and task worktrees are never used.",
                    "Switch repository",
                ));
        }

        let header = self.header(
            theme,
            HeaderData {
                repo: header_repo,
                agent: session
                    .as_ref()
                    .map(|session| session.agent.as_str())
                    .or(chosen.as_deref()),
                cwd: cwd.as_ref().or(repo_path.as_ref()),
                session_id: session_id.as_deref(),
                session: session.as_ref(),
                exit: exit.as_ref(),
                starting,
                closing,
                start_error: start_error.as_deref(),
                close_error: close_error.as_deref(),
            },
            narrow,
            cx,
        );
        let session_ended =
            exit.is_some() || session.as_ref().is_some_and(|session| !session.running);
        let start_form = (!has_session || session_ended).then(|| {
            self.start_form(
                theme,
                StartFormData {
                    agents,
                    chosen: chosen.as_deref(),
                    cwd: cwd.as_ref().or(repo_path.as_ref()),
                    model_focused,
                    error: start_error.as_deref(),
                },
                cx,
            )
        });

        let empty = if starting {
            Some(components::empty_state(
                theme,
                "◐",
                "AGENT",
                "Starting the session…",
                "The daemon is opening the agent in the active repository's main checkout.",
                "Waiting for the agent",
            ))
        } else {
            None
        };

        let body: AnyElement = if has_session {
            self.conversation.clone().into_any_element()
        } else if let Some(empty) = empty {
            empty.into_any_element()
        } else {
            components::empty_state(
                theme,
                "✦",
                "AGENT",
                "No agent session yet",
                "Pick an available agent above, optionally constrain its model, then start. The conversation, plan, permissions and session options appear here without a task.",
                "Start a session",
            )
            .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.colors.surface)
            .child(header)
            .when_some(start_form, |surface, form| surface.child(form))
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
    }
}
