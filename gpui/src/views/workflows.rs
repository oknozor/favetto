//! Workflows: one root's DAG, progress and per-node actions.

use gpui::{Context, Entity, IntoElement, Render, Window};

use crate::components;
use crate::routes::Route;
use crate::state::ClientState;
use crate::theme::Theme;

/// The Workflows surface.
pub struct WorkflowsView {
    _state: Entity<ClientState>,
}

impl WorkflowsView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self { _state: state }
    }
}

impl Render for WorkflowsView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        components::empty_state(
            theme,
            Route::Workflows.icon(),
            "DAG",
            Route::Workflows.empty_title(),
            Route::Workflows.empty_body(),
            Route::Workflows.primary_hint(),
        )
    }
}
