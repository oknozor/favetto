//! The detail region: a third column on wide windows, a pushed page on narrow
//! ones (docs/design/gpui-client.md §1).

use gpui::{div, prelude::*, px, Context, Entity, FocusHandle, IntoElement, Render, Window};

use crate::components;
use crate::inbox::{self, StatusKind};
use crate::layout::{container_query, LayoutMode};
use crate::routes::Route;
use crate::state::ClientState;
use crate::theme::Theme;

/// The detail surface. It reads the active route so the placeholder copy names
/// the right kind of selection (run, definition, schedule).
pub struct DetailView {
    state: Entity<ClientState>,
    back_focus: FocusHandle,
}

impl DetailView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self {
            state,
            back_focus: cx.focus_handle().tab_index(20).tab_stop(true),
        }
    }
}

impl Render for DetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        let route = self.state.read(cx).active_route();
        let narrow = container_query(window.viewport_size().width) == LayoutMode::Narrow;
        let (icon, eyebrow, title, body) = detail_copy(route);

        // Step 04 wires `Enter` to the selected run; the full header and tabs
        // arrive with step 06, so echo the run here to make the action visible.
        let selected = self
            .state
            .read(cx)
            .selected_task()
            .map(|task| (task.name.clone(), task.status, task.attempt));
        let (icon, title, body) = match selected {
            Some((name, status, attempt)) if matches!(route, Route::Inbox | Route::Tasks) => {
                let status = StatusKind::from_task_status(status);
                (
                    status.icon(),
                    inbox::task_display_name(&name).to_string(),
                    format!(
                        "{} · attempt {attempt}. Output, events and runs arrive with the task-detail step.",
                        status.label()
                    ),
                )
            }
            _ => (icon, title.to_string(), body.to_string()),
        };

        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(theme.spacing.s2))
            .h(px(44.0))
            .px(px(theme.spacing.s3))
            .border_b_1()
            .border_color(theme.colors.border)
            .child(components::section_label(theme, "DETAIL"))
            .when(narrow, |header| {
                let state = self.state.clone();
                header.child(
                    components::with_focus(
                        div()
                            .id("detail-back")
                            .px(px(theme.spacing.s2))
                            .py(px(2.0))
                            .rounded(px(theme.radii.r4))
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.text)
                            .cursor_pointer()
                            .track_focus(&self.back_focus)
                            .child("← Back"),
                        theme,
                    )
                    .on_click(cx.listener(
                        move |_this, _event, _window, cx| {
                            state.update(cx, |state, cx| {
                                state.close_detail();
                                cx.notify();
                            });
                        },
                    )),
                )
            });

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.colors.surface)
            .child(header)
            .child(components::empty_state(
                theme,
                icon,
                eyebrow,
                &title,
                &body,
                "Open a row to inspect it",
            ))
    }
}

fn detail_copy(route: Route) -> (&'static str, &'static str, &'static str, &'static str) {
    match route {
        Route::Inbox | Route::Tasks => (
            "◎",
            "RUN",
            "No run selected",
            "Open a run in the list to see its status, agent and attempt here. Output, Events and Runs stay one tab away.",
        ),
        Route::Catalog => (
            Route::Catalog.icon(),
            "DEFINITION",
            "No definition selected",
            "Choose a definition to preview its declared variables and launch form here.",
        ),
        Route::Workflows => (
            Route::Workflows.icon(),
            "ROOT",
            "No workflow selected",
            "Choose a root run to inspect its DAG, completed steps and per-node progress here.",
        ),
        Route::Schedules => (
            Route::Schedules.icon(),
            "TRIGGER",
            "No schedule selected",
            "Choose a schedule to see its cadence, next fire time and last run here.",
        ),
        Route::Settings => (
            Route::Settings.icon(),
            "CONNECTION",
            "Current connection",
            "The settings surface owns editing. This column keeps the endpoint and token source visible while you work.",
        ),
    }
}
