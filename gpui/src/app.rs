//! Root view: the three-region shell from docs/design/gpui-client.md §4
//! (navigation / working list / detail) with the six navigation entries
//! (Inbox → Tasks → Catalog → Workflows → Schedules → Settings).
//!
//! Step 01 delivered the skeleton; step 02 adds the live connection state:
//! the [`ClientState`] entity owns the transport, and the status bar renders the
//! connection label, the last `system.ping` round trip, and a Reconnect action.
//! Later steps fill in the real routes (§1 surfaces table).

use gpui::{div, prelude::*, px, Context, Div, Entity, IntoElement, Rgba, Stateful, Window};

use crate::state::{ClientState, ConnectionState};
use crate::theme::{Theme, SIDEBAR_WIDTH, STATUS_BAR_HEIGHT};

/// Routes of the root shell, mirroring the daemon vocabulary (docs §1; "Jakob's
/// Law" in §2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Route {
    /// What needs me now — the Inbox surface.
    #[default]
    Inbox,
    /// All runs, filter and search — the Tasks surface.
    Tasks,
    /// Find and launch a definition — the Catalog surface.
    Catalog,
    /// One root's DAG — the Workflows surface.
    Workflows,
    /// Recurring triggers — the Schedules surface.
    Schedules,
    /// Connection, agents, theme — the Settings surface.
    Settings,
}

impl Route {
    /// Navigation order (docs §1, Serial Position Effect): the two surfaces
    /// users need most come first, configuration is last.
    pub const ALL: [Route; 6] = [
        Route::Inbox,
        Route::Tasks,
        Route::Catalog,
        Route::Workflows,
        Route::Schedules,
        Route::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Route::Inbox => "Inbox",
            Route::Tasks => "Tasks",
            Route::Catalog => "Catalog",
            Route::Workflows => "Workflows",
            Route::Schedules => "Schedules",
            Route::Settings => "Settings",
        }
    }
}

/// Root view: the three bounded regions, separated by 1px borders (Law of
/// Common Region, docs §2).
pub struct RootView {
    route: Route,
    state: Entity<ClientState>,
}

impl RootView {
    /// Build the shell and start the transport state.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let state = cx.new(ClientState::new);
        // Re-render whenever the connection, ping or feed changes.
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self {
            route: Route::default(),
            state,
        }
    }

    /// One sidebar entry; clicking it switches the active route.
    fn nav_entry(&self, route: Route, theme: &Theme, cx: &mut Context<Self>) -> Stateful<Div> {
        let active = route == self.route;
        div()
            .id(route.label())
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .w_full()
            .p(px(theme.spacing.s2))
            .rounded(px(theme.radii.r4))
            .when(active, |s| s.bg(theme.colors.accent))
            .text_color(if active {
                theme.colors.text
            } else {
                theme.colors.muted
            })
            .text_size(px(theme.font.sm))
            .cursor_pointer()
            .child(route.label())
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.route = route;
                cx.notify();
            }))
    }

    /// Placeholder for the working-list region (docs §4: routes are filled
    /// in step by step).
    fn content_placeholder(&self, theme: &Theme) -> Div {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .justify_center()
            .items_center()
            .gap(px(theme.spacing.s3))
            .rounded(px(theme.radii.r8))
            .border_1()
            .border_color(theme.colors.border)
            .text_color(theme.colors.muted)
            .child(
                div()
                    .text_size(px(theme.font.xl))
                    .text_color(theme.colors.text)
                    .child(self.route.label()),
            )
            .child(
                div()
                    .mt(px(theme.spacing.s1))
                    .text_size(px(theme.font.lg))
                    .child("Skeleton shell for this route (step 01)."),
            )
    }

    /// The temporary status line: connection state, ping round trip, cursor and
    /// a Reconnect action (step 02's deliverable; step 03 replaces it with the
    /// real status bar).
    fn status_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        // Clone what the status line renders and release the entity borrow before
        // building elements that need the context.
        let (connection, status_color, endpoint, ping, feed, error) = {
            let state = self.state.read(cx);
            let connection = state.connection().clone();
            (
                connection.clone(),
                connection_color(&connection, theme),
                state.config().endpoint.clone(),
                state.ping_label(),
                feed_label(state),
                state.last_error().map(str::to_string),
            )
        };

        let reconnect_state = self.state.clone();
        let reconnect = div()
            .id("reconnect")
            .px(px(theme.spacing.s2))
            .py(px(theme.spacing.s1))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(theme.colors.border)
            .text_color(theme.colors.text)
            .cursor_pointer()
            .child("Reconnect")
            .on_click(cx.listener(move |_this, _event, _window, cx| {
                reconnect_state.update(cx, |state, cx| {
                    state.reconnect();
                    cx.notify();
                });
            }));

        let left = div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .min_w_0()
            .child(
                div()
                    .w(px(8.0))
                    .h(px(8.0))
                    .rounded(px(4.0))
                    .bg(status_color),
            )
            .child(
                div()
                    .text_color(theme.colors.text)
                    .child(connection.label()),
            )
            .child(
                div()
                    .text_color(theme.colors.muted)
                    .child(format!("· {endpoint}")),
            )
            .when_some(feed, |left, feed| {
                left.child(
                    div()
                        .text_color(theme.colors.muted)
                        .child(format!("· {feed}")),
                )
            })
            .when_some(error, |left, error| {
                left.child(div().text_color(theme.colors.danger).child(error))
            });

        let right = div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s3))
            .child(div().text_color(theme.colors.muted).child(ping))
            .child(reconnect);

        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(STATUS_BAR_HEIGHT))
            .px(px(theme.spacing.s4))
            .rounded(px(theme.radii.r12))
            .border_t_1()
            .border_color(theme.colors.border)
            .text_color(theme.colors.muted)
            .text_size(px(theme.font.xs))
            .child(left)
            .child(right)
    }
}

/// The status-dot color for a connection state (never the only signal — the
/// label is always rendered too).
fn connection_color(connection: &ConnectionState, theme: &Theme) -> Rgba {
    match connection {
        ConnectionState::Connected => theme.colors.success,
        ConnectionState::Connecting | ConnectionState::Reconnecting { .. } => theme.colors.warning,
        ConnectionState::Unauthorized { .. } | ConnectionState::Disconnected { .. } => {
            theme.colors.danger
        }
    }
}

/// The push/cursor summary shown after the endpoint.
fn feed_label(state: &ClientState) -> Option<String> {
    let cursor = state.cursor().map(|cursor| format!("cursor {cursor}"));
    let last = state.last_push().map(str::to_string);
    match (cursor, last) {
        (Some(cursor), Some(last)) => Some(format!("{cursor} · {last}")),
        (Some(cursor), None) => Some(cursor),
        (None, Some(last)) => Some(last),
        (None, None) => None,
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());

        let sidebar = div()
            .flex_col()
            .w(px(SIDEBAR_WIDTH))
            .p(px(theme.spacing.s4))
            .gap(px(theme.spacing.s6))
            .border_r_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
            .children(
                Route::ALL
                    .iter()
                    .map(|route| self.nav_entry(*route, theme, cx)),
            );

        let content = self.content_placeholder(theme);
        let status_bar = self.status_bar(theme, cx);

        div()
            .flex()
            .flex_col()
            .h_full()
            .bg(theme.colors.bg)
            .text_color(theme.colors.text)
            .text_size(px(theme.font.base))
            .child(sidebar)
            .child(content)
            .child(status_bar)
    }
}
