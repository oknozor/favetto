//! Schedules: recurring triggers in a table plus create/edit and delete sheets.
//!
//! Cron parsing/normalization and the plain-language hint live in
//! [`crate::schedules`] (pure and unit-tested). This file only renders the
//! model. Keyboard is owned by the shell: `n` creates, `↵` edits, `d` opens the
//! delete confirmation, Tab/Shift+Tab walk the sheet and `⌘↵` saves.

use chrono::Utc;
use favetto_wire::model::Schedule;
use gpui::{
    div, prelude::*, px, AnyElement, Context, Div, Entity, FocusHandle, Hsla, IntoElement, Render,
    ScrollHandle, Stateful, Window,
};

use crate::components;
use crate::routes::Route;
use crate::schedules::{self, ScheduleEditor, ScheduleField, SchedulesStatus};
use crate::state::ClientState;
use crate::theme::Theme;

/// The Schedules surface.
pub struct SchedulesView {
    state: Entity<ClientState>,
    table_scroll: ScrollHandle,
    scrolled_to: Option<String>,
    new_focus: FocusHandle,
    refresh_focus: FocusHandle,
    retry_focus: FocusHandle,
    save_focus: FocusHandle,
    cancel_focus: FocusHandle,
}

impl SchedulesView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self {
            state,
            table_scroll: ScrollHandle::new(),
            scrolled_to: None,
            new_focus: cx.focus_handle().tab_index(40).tab_stop(true),
            refresh_focus: cx.focus_handle().tab_index(41).tab_stop(true),
            retry_focus: cx.focus_handle().tab_index(42).tab_stop(true),
            save_focus: cx.focus_handle().tab_index(43).tab_stop(true),
            cancel_focus: cx.focus_handle().tab_index(44).tab_stop(true),
        }
    }

    fn header(&self, theme: &Theme, count: usize, cx: &mut Context<Self>) -> Div {
        let refresh = components::with_focus(
            div()
                .id("schedules-refresh")
                .h(px(36.0))
                .px(px(theme.spacing.s3))
                .flex()
                .items_center()
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(theme.colors.border)
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.refresh_focus)
                .hover(|style| style.bg(theme.colors.surface))
                .child("Refresh")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.refresh_schedules();
                        state.refresh_catalog();
                        cx.notify();
                    });
                })),
            theme,
        );

        let new_schedule = components::with_focus(
            div()
                .id("schedules-new")
                .h(px(40.0))
                .px(px(theme.spacing.s4))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .rounded(px(theme.radii.r8))
                .bg(Hsla::from(theme.colors.accent).opacity(0.18))
                .text_size(px(theme.font.sm))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.colors.accent)
                .cursor_pointer()
                .track_focus(&self.new_focus)
                .child("New schedule")
                .child(components::keycap(theme, "n"))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.open_schedule_new();
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
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .child(components::section_label(theme, "SCHEDULES"))
                            .child(hint(theme, &format!("{count} total"))),
                    )
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child("Recurring triggers"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s1))
                            .child(components::keycap(theme, "↵"))
                            .child(hint(theme, "edit"))
                            .child(components::keycap(theme, "d"))
                            .child(hint(theme, "delete")),
                    )
                    .child(refresh)
                    .child(new_schedule),
            )
    }

    fn table_header(&self, theme: &Theme) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s3))
            .h(px(28.0))
            .px(px(theme.spacing.s3))
            .border_b_1()
            .border_color(theme.colors.border)
            .text_size(px(theme.font.xs))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(theme.colors.muted)
            .child(div().w(px(64.0)).flex_shrink_0().child("ENABLED"))
            .child(div().w(px(172.0)).flex_shrink_0().child("CRON"))
            .child(div().flex_1().min_w_0().child("TASK"))
            .child(div().w(px(224.0)).flex_shrink_0().child("NEXT RUN"))
            .child(div().w(px(92.0)).flex_shrink_0())
    }

    fn row(
        &self,
        theme: &Theme,
        index: usize,
        schedule: &Schedule,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = schedule.id.clone();
        let edit_id = schedule.id.clone();
        let delete_id = schedule.id.clone();
        let enabled = schedule.enabled;
        let (dot, state_label) = if enabled {
            ("●", "on")
        } else {
            ("○", "off")
        };
        let color = if enabled {
            theme.colors.success
        } else {
            theme.colors.muted
        };

        div()
            .id(("schedule-row", index))
            .flex()
            .items_center()
            .gap(px(theme.spacing.s3))
            .min_h(px(44.0))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s1))
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
            .cursor_pointer()
            .hover(|style| style.bg(theme.colors.bg))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.select_schedule(&id);
                    state.open_schedule_selected(cx);
                    cx.notify();
                });
            }))
            .child(
                div()
                    .w(px(64.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s1))
                    .text_size(px(theme.font.xs))
                    .text_color(color)
                    .child(dot)
                    .child(
                        div()
                            .text_color(if enabled {
                                theme.colors.text
                            } else {
                                theme.colors.muted
                            })
                            .child(state_label),
                    ),
            )
            .child(
                div()
                    .w(px(172.0))
                    .flex_shrink_0()
                    .truncate()
                    .font_family("monospace")
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.text)
                    .child(schedule.cron.clone()),
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
                            .truncate()
                            .text_size(px(theme.font.sm))
                            .text_color(theme.colors.text)
                            .child(inbox_task_label(&schedule.task)),
                    )
                    .child(
                        div()
                            .truncate()
                            .font_family("monospace")
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child(schedule.task.clone()),
                    ),
            )
            .child(
                div()
                    .w(px(224.0))
                    .flex_shrink_0()
                    .truncate()
                    .text_size(px(theme.font.xs))
                    .text_color(if enabled {
                        theme.colors.text
                    } else {
                        theme.colors.muted
                    })
                    .child(schedules::next_run_label(schedule, Utc::now())),
            )
            .child(
                div()
                    .w(px(92.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(theme.spacing.s1))
                    .child(
                        div()
                            .id(("schedule-edit", index))
                            .px(px(theme.spacing.s2))
                            .py(px(3.0))
                            .rounded(px(theme.radii.r4))
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.accent)
                            .cursor_pointer()
                            .hover(|style| style.bg(Hsla::from(theme.colors.accent).opacity(0.14)))
                            .child("Edit")
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.select_schedule(&edit_id);
                                    state.open_schedule_selected(cx);
                                    cx.notify();
                                });
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        div()
                            .id(("schedule-delete", index))
                            .px(px(theme.spacing.s2))
                            .py(px(3.0))
                            .rounded(px(theme.radii.r4))
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .cursor_pointer()
                            .hover(|style| style.bg(Hsla::from(theme.colors.danger).opacity(0.14)))
                            .child("Delete")
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.request_schedule_delete(&delete_id, cx);
                                    cx.notify();
                                });
                                cx.stop_propagation();
                            })),
                    ),
            )
    }

    fn error_banner(&self, theme: &Theme, message: &str, cx: &mut Context<Self>) -> Div {
        let retry = components::with_focus(
            div()
                .id("schedules-retry")
                .px(px(theme.spacing.s2))
                .py(px(3.0))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(theme.colors.border)
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.retry_focus)
                .child("Retry")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.refresh_schedules();
                        cx.notify();
                    });
                })),
            theme,
        );

        div()
            .mx(px(theme.spacing.s3))
            .mt(px(theme.spacing.s2))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s2))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(theme.spacing.s2))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(Hsla::from(theme.colors.danger).opacity(0.5))
            .bg(Hsla::from(theme.colors.danger).opacity(0.10))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(format!("Could not load schedules: {message}")),
            )
            .child(retry)
    }

    fn editor_sheet(
        &self,
        theme: &Theme,
        editor: &ScheduleEditor,
        catalog: &[(String, Option<String>)],
        cx: &mut Context<Self>,
    ) -> Div {
        let cron_focused = editor.focus == ScheduleField::Cron;
        let cron_box = div()
            .id("schedule-cron")
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .h(px(40.0))
            .px(px(theme.spacing.s3))
            .rounded(px(theme.radii.r4))
            .border_2()
            .border_color(if cron_focused {
                theme.colors.accent
            } else if editor.cron_error.is_some() {
                theme.colors.danger
            } else {
                theme.colors.border
            })
            .bg(theme.colors.bg)
            .cursor_text()
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.focus_schedule_field(ScheduleField::Cron);
                    cx.notify();
                });
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family("monospace")
                    .text_size(px(theme.font.sm))
                    .text_color(if editor.cron.is_empty() {
                        theme.colors.muted
                    } else {
                        theme.colors.text
                    })
                    .child(if editor.cron.is_empty() {
                        "0 0 9 * * *".to_string()
                    } else {
                        editor.cron.clone()
                    }),
            )
            .when(cron_focused, |field| {
                field.child(div().text_color(theme.colors.accent).child("▌"))
            });

        let mut cron_field = div()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s1))
            .child(field_label(theme, "Cron expression", true))
            .child(cron_box);
        if let Some(error) = &editor.cron_error {
            cron_field = cron_field.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.danger)
                    .child(error.clone()),
            );
        } else if let Some(hint_text) = &editor.hint {
            cron_field = cron_field.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.accent)
                    .child(hint_text.clone()),
            );
        } else {
            cron_field = cron_field.child(hint(
                theme,
                "Six fields (second minute hour day month weekday) or five fields (seconds = 0). Time is UTC.",
            ));
        }

        let task_rows: Vec<AnyElement> = if catalog.is_empty() {
            vec![div()
                .p(px(theme.spacing.s3))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .child(
                    "The catalog is still loading. A schedule needs a catalog task; refresh the Catalog surface if this stays empty.",
                )
                .into_any_element()]
        } else {
            catalog
                .iter()
                .enumerate()
                .map(|(task_index, (entry_name, agent))| {
                    let selected = entry_name == &editor.task;
                    let name = entry_name.clone();
                    div()
                        .id(("schedule-task", task_index))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(theme.spacing.s2))
                        .h(px(30.0))
                        .px(px(theme.spacing.s2))
                        .rounded(px(theme.radii.r4))
                        .when(selected, |row| {
                            row.bg(Hsla::from(theme.colors.accent).opacity(0.12))
                                .border_1()
                                .border_color(theme.colors.accent)
                        })
                        .hover(|style| style.bg(theme.colors.surface))
                        .cursor_pointer()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_family("monospace")
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.text)
                                .child(entry_name.clone()),
                        )
                        .when_some(agent.clone(), |row, agent| {
                            row.child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(theme.font.xs))
                                    .text_color(theme.colors.muted)
                                    .child(agent),
                            )
                        })
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.state.update(cx, |state, cx| {
                                state.select_schedule_task(&name);
                                cx.notify();
                            });
                        }))
                        .into_any_element()
                })
                .collect()
        };

        let task_known = catalog.iter().any(|(name, _)| name == &editor.task);
        let mut task_field = div()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s1))
            .child(field_label(theme, "Task", false))
            .child(
                div()
                    .id("schedule-tasks")
                    .max_h(px(150.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .p(px(theme.spacing.s1))
                    .rounded(px(theme.radii.r4))
                    .border_1()
                    .border_color(if editor.focus == ScheduleField::Task {
                        theme.colors.accent
                    } else {
                        theme.colors.border
                    })
                    .bg(theme.colors.bg)
                    .children(task_rows),
            );
        if let Some(error) = &editor.task_error {
            task_field = task_field.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.danger)
                    .child(error.clone()),
            );
        } else if !editor.task.is_empty() && !task_known {
            task_field = task_field.child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.warning)
                    .child(format!(
                        "'{}' is not in the current catalog; pick a task above or keep it if the definition is temporarily missing.",
                        editor.task
                    )),
            );
        }

        let enabled_color = if editor.enabled {
            theme.colors.success
        } else {
            theme.colors.muted
        };
        let enabled = div()
            .id("schedule-enabled")
            .flex()
            .items_center()
            .justify_between()
            .h(px(38.0))
            .px(px(theme.spacing.s3))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(if editor.focus == ScheduleField::Enabled {
                theme.colors.accent
            } else {
                theme.colors.border
            })
            .bg(theme.colors.bg)
            .cursor_pointer()
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.toggle_schedule_enabled();
                    cx.notify();
                });
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s1))
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(div().text_color(enabled_color).child(if editor.enabled {
                        "●"
                    } else {
                        "○"
                    }))
                    .child(if editor.enabled { "Enabled" } else { "Paused" }),
            )
            .child(hint(theme, "⌘E toggles"));

        let save_label = if editor.submitting {
            "Saving…"
        } else if editor.is_edit() {
            "Save changes"
        } else {
            "Create schedule"
        };
        let save = components::with_focus(
            div()
                .id("schedule-save")
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
                .child(save_label)
                .child(components::keycap(theme, "⌘↵"))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, _cx| {
                        state.submit_schedule();
                    });
                    cx.notify();
                })),
            theme,
        );
        let cancel = components::with_focus(
            div()
                .id("schedule-cancel")
                .h(px(36.0))
                .px(px(theme.spacing.s3))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(theme.colors.border)
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.cancel_focus)
                .child("Cancel")
                .child(components::keycap(theme, "esc"))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.close_schedule_editor();
                        cx.notify();
                    });
                })),
            theme,
        );

        let server_error = editor.server_error.clone().map(|message| {
            div()
                .flex()
                .items_start()
                .gap(px(theme.spacing.s2))
                .px(px(theme.spacing.s3))
                .py(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(Hsla::from(theme.colors.danger).opacity(0.5))
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
                        .text_size(px(theme.font.sm))
                        .text_color(theme.colors.text)
                        .child(message),
                )
        });

        let panel = div()
            .w(px(620.0))
            .max_h(px(700.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(theme.radii.r12))
            .border_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(theme.spacing.s1))
                    .p(px(theme.spacing.s4))
                    .border_b_1()
                    .border_color(theme.colors.border)
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child(if editor.is_edit() {
                                "Edit schedule"
                            } else {
                                "New schedule"
                            }),
                    )
                    .child(hint(
                        theme,
                        "The daemon computes the next run when it stores the schedule.",
                    )),
            )
            .child(
                div()
                    .id("schedule-sheet-fields")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(theme.spacing.s3))
                    .p(px(theme.spacing.s4))
                    .child(cron_field)
                    .child(task_field)
                    .child(enabled)
                    .when_some(server_error, |body, error| body.child(error)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(theme.spacing.s3))
                    .p(px(theme.spacing.s3))
                    .border_t_1()
                    .border_color(theme.colors.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .child(components::keycap(theme, "tab"))
                            .child(hint(theme, "next field"))
                            .child(components::keycap(theme, "⌘↵"))
                            .child(hint(theme, "save")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .child(cancel)
                            .child(save),
                    ),
            );

        overlay(theme, panel)
    }

    fn loading_body(&self, theme: &Theme) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s3))
            .children((0..5).map(|index| skeleton_row(theme, 140.0 + index as f32 * 18.0)))
    }

    fn error_body(&self, theme: &Theme, message: &str, cx: &mut Context<Self>) -> Div {
        let retry = components::with_focus(
            div()
                .id("schedules-error-retry")
                .mt(px(theme.spacing.s2))
                .h(px(36.0))
                .px(px(theme.spacing.s3))
                .flex()
                .items_center()
                .rounded(px(theme.radii.r4))
                .bg(Hsla::from(theme.colors.accent).opacity(0.14))
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.accent)
                .cursor_pointer()
                .track_focus(&self.retry_focus)
                .child("Retry")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.refresh_schedules();
                        cx.notify();
                    });
                })),
            theme,
        );
        div()
            .flex_1()
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
                    .child("Schedules are unavailable"),
            )
            .child(
                div()
                    .max_w(px(520.0))
                    .text_center()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.muted)
                    .child(message.to_string()),
            )
            .child(retry)
    }
}

impl Render for SchedulesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.state.read(cx).theme(window.appearance());
        let (status, rows, selected, editor, catalog) = {
            let state = self.state.read(cx);
            (
                state.schedules().status().clone(),
                state.schedules().schedules().to_vec(),
                state.schedule_selected_id(),
                state.schedules().editor().cloned(),
                state
                    .catalog()
                    .entries()
                    .iter()
                    .map(|entry| (entry.name.clone(), entry.agent.clone()))
                    .collect::<Vec<_>>(),
            )
        };

        if selected != self.scrolled_to {
            if let Some(index) = selected
                .as_ref()
                .and_then(|id| rows.iter().position(|row| &row.id == id))
            {
                self.table_scroll.scroll_to_item(index);
            }
            self.scrolled_to = selected.clone();
        }

        let header = self.header(theme, rows.len(), cx);
        let body = if matches!(status, SchedulesStatus::Loading) && rows.is_empty() {
            self.loading_body(theme).into_any_element()
        } else if let SchedulesStatus::Failed { message } = &status {
            if rows.is_empty() {
                self.error_body(theme, message, cx).into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(self.error_banner(theme, message, cx))
                    .child(self.table(theme, &rows, selected.as_deref(), cx))
                    .into_any_element()
            }
        } else {
            self.table(theme, &rows, selected.as_deref(), cx)
                .into_any_element()
        };

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.colors.bg)
            .child(header)
            .child(body);
        if let Some(editor) = &editor {
            root = root.child(self.editor_sheet(theme, editor, &catalog, cx));
        }
        root
    }
}

impl SchedulesView {
    fn table(
        &self,
        theme: &Theme,
        rows: &[Schedule],
        selected: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Div {
        if rows.is_empty() {
            return div()
                .flex_1()
                .min_h_0()
                .child(components::empty_state(
                    theme,
                    Route::Schedules.icon(),
                    "TRIGGERS",
                    Route::Schedules.empty_title(),
                    "A schedule needs a cron expression and a catalog task. Create one here; the daemon stores it and computes the next run.",
                    Route::Schedules.primary_hint(),
                ));
        }
        let mut scroll = div()
            .id("schedules-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.table_scroll)
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(theme.spacing.s2));
        for (index, row) in rows.iter().enumerate() {
            scroll =
                scroll.child(self.row(theme, index, row, selected == Some(row.id.as_str()), cx));
        }
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.table_header(theme))
            .child(scroll)
    }
}

fn overlay(theme: &Theme, panel: Div) -> Div {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(Hsla::from(theme.colors.bg).opacity(0.86))
        .child(panel)
}

fn hint(theme: &Theme, text: &str) -> Div {
    div()
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child(text.to_string())
}

fn field_label(theme: &Theme, label: &str, required: bool) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(theme.spacing.s1))
        .text_size(px(theme.font.sm))
        .text_color(theme.colors.text)
        .child(label.to_string())
        .when(required, |row| {
            row.child(div().text_color(theme.colors.danger).child("*"))
        })
}

/// `repo:favetto/triage` → `triage`, keeping the full name beside it.
fn inbox_task_label(task: &str) -> String {
    task.rsplit('/').next().unwrap_or(task).to_string()
}

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
                .h(px(24.0))
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
