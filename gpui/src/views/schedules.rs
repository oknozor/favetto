//! Schedules: recurring triggers and their next fire time.

use gpui::{Context, Entity, IntoElement, Render, Window};

use crate::components;
use crate::routes::Route;
use crate::state::ClientState;
use crate::theme::Theme;

/// The Schedules surface.
pub struct SchedulesView {
    _state: Entity<ClientState>,
}

impl SchedulesView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self { _state: state }
    }
}

impl Render for SchedulesView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        components::empty_state(
            theme,
            Route::Schedules.icon(),
            "TRIGGERS",
            Route::Schedules.empty_title(),
            Route::Schedules.empty_body(),
            Route::Schedules.primary_hint(),
        )
    }
}
