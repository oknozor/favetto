//! Toast notifications for action feedback.
//!
//! A toast is a short, dismissible status message shown over the current view.
//! The caller owns its lifetime: it renders the [`Toast`] while a message is
//! set and clears the signal from `on_dismiss` to hide it.

use dioxus::prelude::*;

/// The tone of a [`Toast`], which drives its colour and ARIA role.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToastKind {
    /// The action completed.
    Success,
    /// The action failed.
    Error,
    /// Neutral information.
    Info,
}

impl ToastKind {
    /// The CSS classes carrying the tone (including the base `toast` class).
    pub fn class(self) -> &'static str {
        match self {
            ToastKind::Success => "toast toast-success",
            ToastKind::Error => "toast toast-error",
            ToastKind::Info => "toast toast-info",
        }
    }

    /// The ARIA role: failures are assertive, the rest polite.
    pub fn role(self) -> &'static str {
        match self {
            ToastKind::Error => "alert",
            ToastKind::Success | ToastKind::Info => "status",
        }
    }
}

/// A dismissible status message, fixed above the view.
#[component]
pub fn Toast(kind: ToastKind, message: String, on_dismiss: EventHandler<()>) -> Element {
    rsx! {
        div { class: kind.class(), role: kind.role(),
            span { class: "toast-message", "{message}" }
            button {
                class: "toast-dismiss",
                aria_label: "Dismiss",
                onclick: move |_| on_dismiss.call(()),
                "×"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_map_to_distinct_classes_and_roles() {
        assert_eq!(ToastKind::Success.class(), "toast toast-success");
        assert_eq!(ToastKind::Error.class(), "toast toast-error");
        assert_eq!(ToastKind::Info.class(), "toast toast-info");
        // Only failures are assertive.
        assert_eq!(ToastKind::Error.role(), "alert");
        assert_eq!(ToastKind::Success.role(), "status");
        assert_eq!(ToastKind::Info.role(), "status");
    }

    #[test]
    fn toast_mounts_host_side() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                Toast {
                    kind: ToastKind::Success,
                    message: "Reply sent.".to_string(),
                    on_dismiss: move |_| {},
                }
            }
        });
        dom.rebuild_in_place();
    }
}
