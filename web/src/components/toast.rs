//! Toast notifications for action feedback.
//!
//! A toast is a short, dismissible status message shown over the current view.
//! The caller owns its lifetime: it renders the [`Toast`] while a message is
//! set and clears the signal from `on_dismiss` to hide it.

use dioxus::prelude::*;

use crate::components::icon::{AppIcon, AppIconKind};

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

    /// The decorative icon for the tone.
    pub fn icon(self) -> AppIconKind {
        match self {
            ToastKind::Success => AppIconKind::ToastSuccess,
            ToastKind::Error => AppIconKind::ToastError,
            ToastKind::Info => AppIconKind::ToastInfo,
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
            AppIcon { kind: kind.icon(), class: "toast-icon" }
            span { class: "toast-message", "{message}" }
            button {
                class: "toast-dismiss",
                aria_label: "Dismiss",
                onclick: move |_| on_dismiss.call(()),
                AppIcon { kind: AppIconKind::Close, class: "toast-dismiss-icon" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_map_to_distinct_classes_roles_and_icons() {
        assert_eq!(ToastKind::Success.class(), "toast toast-success");
        assert_eq!(ToastKind::Error.class(), "toast toast-error");
        assert_eq!(ToastKind::Info.class(), "toast toast-info");
        // Only failures are assertive.
        assert_eq!(ToastKind::Error.role(), "alert");
        assert_eq!(ToastKind::Success.role(), "status");
        assert_eq!(ToastKind::Info.role(), "status");
        // Each tone has its own icon.
        assert_ne!(ToastKind::Success.icon(), ToastKind::Error.icon());
        assert_ne!(ToastKind::Error.icon(), ToastKind::Info.icon());
        assert_ne!(ToastKind::Success.icon(), ToastKind::Info.icon());
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
