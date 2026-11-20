//! Catalog: browse definitions, preview variables and launch.

use gpui::{Context, Entity, IntoElement, Render, Window};

use crate::components;
use crate::routes::Route;
use crate::state::ClientState;
use crate::theme::Theme;

/// The Catalog surface.
pub struct CatalogView {
    state: Entity<ClientState>,
}

impl CatalogView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self { state }
    }
}

impl Render for CatalogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        let connected = self.state.read(cx).connection().is_connected();

        let (title, body) = if connected {
            (
                Route::Catalog.empty_title().to_string(),
                Route::Catalog.empty_body().to_string(),
            )
        } else {
            (
                "Waiting for the daemon".to_string(),
                "Catalog definitions arrive over the authenticated RPC connection. Reconnect from the status bar or the command palette once the daemon is running.".to_string(),
            )
        };

        components::empty_state(
            theme,
            Route::Catalog.icon(),
            "DEFINITIONS",
            &title,
            &body,
            Route::Catalog.primary_hint(),
        )
    }
}
