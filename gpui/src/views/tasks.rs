//! Tasks: all runs, filter and search.

use gpui::{Context, Entity, IntoElement, Render, Window};

use crate::components;
use crate::routes::Route;
use crate::state::ClientState;
use crate::theme::Theme;

/// The Tasks surface.
pub struct TasksView {
    _state: Entity<ClientState>,
}

impl TasksView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self { _state: state }
    }
}

impl Render for TasksView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        components::empty_state(
            theme,
            Route::Tasks.icon(),
            "ALL RUNS",
            Route::Tasks.empty_title(),
            Route::Tasks.empty_body(),
            Route::Tasks.primary_hint(),
        )
    }
}
