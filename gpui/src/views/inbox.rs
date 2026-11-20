//! Inbox: the "what needs me now?" surface.

use gpui::{div, prelude::*, px, Context, Entity, IntoElement, Render, Window};

use crate::components;
use crate::routes::Route;
use crate::state::ClientState;
use crate::theme::Theme;

/// The Inbox surface.
pub struct InboxView {
    state: Entity<ClientState>,
}

impl InboxView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self { state }
    }
}

impl Render for InboxView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        let awaiting = self.state.read(cx).awaiting_count();

        let (title, body) = if awaiting == 0 {
            (
                Route::Inbox.empty_title().to_string(),
                Route::Inbox.empty_body().to_string(),
            )
        } else {
            (
                format!("{awaiting} runs need a decision"),
                "Open a blocked run to answer its permission prompt, or retry a failure without leaving the inbox.".to_string(),
            )
        };

        components::empty_state(
            theme,
            Route::Inbox.icon(),
            "NEEDS YOU",
            &title,
            &body,
            Route::Inbox.primary_hint(),
        )
        .child(if awaiting > 0 {
            div()
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.warning)
                .child(format!("{awaiting} awaiting input"))
        } else {
            // Keeps the empty state's vertical rhythm stable when the count is
            // zero; no placeholder text is rendered.
            div().h(px(0.0))
        })
    }
}
