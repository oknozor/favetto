//! Root component and routing.
//!
//! The app is intentionally a shell for now: it mounts the stylesheet and a
//! router outlet with a single placeholder route. The Inbox, Tasks, Catalog and
//! Settings views land in follow-up issues; the route enum is the seam they
//! plug into.

use dioxus::prelude::*;

/// Deep-linkable client routes.
///
/// Deep links matter because `ntfy` pages link straight to work (for example
/// `/inbox/:task_id`); keeping routes in one enum means the daemon and the
/// Android native app share the same navigation vocabulary.
#[derive(Routable, Clone, PartialEq, Debug)]
pub enum Route {
    /// The default view; the Inbox replaces this in a follow-up issue.
    #[route("/")]
    Home {},
}

/// The application root: stylesheet, layout shell, and router outlet.
#[component]
pub fn App() -> Element {
    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/style.css") }
        Router::<Route> {}
    }
}

/// Placeholder home view, replaced by the Inbox at phase 3.2.
#[component]
fn Home() -> Element {
    rsx! {
        main { id: "favetto-app",
            h1 { "favetto" }
            p { "The web client is connected to this daemon." }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The root component mounts without panicking on a host build; this guards
    /// the router/stylesheet wiring that a `dx build` would otherwise be the
    /// first to catch.
    #[test]
    fn app_renders() {
        let mut dom = VirtualDom::new(App);
        dom.rebuild_in_place();
    }

    /// The client links `favetto-wire` directly, so it speaks the daemon's
    /// exact status vocabulary with no re-derivation. This also keeps the path
    /// dependency honest for host `cargo test` runs.
    #[test]
    fn shares_the_wire_status_vocabulary() {
        assert_eq!(
            favetto_wire::model::TaskStatus::Running,
            favetto_wire::model::TaskStatus::Running
        );
    }
}
