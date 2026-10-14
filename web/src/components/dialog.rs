//! Reusable modal dialogs: a generic panel and a yes/no confirmation.
//!
//! Both render a fixed backdrop. Clicking the backdrop dismisses the dialog;
//! the inner panel stops click propagation so a click on the panel (or its
//! controls) does not close it. The panel is labelled by its title and takes
//! focus when it mounts, so Escape closes it from the keyboard.

use dioxus::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counter so every dialog gets a unique `aria-labelledby` target.
static DIALOG_IDS: AtomicU64 = AtomicU64::new(0);

fn next_dialog_id() -> u64 {
    DIALOG_IDS.fetch_add(1, Ordering::Relaxed)
}

/// A generic modal: a titled panel over a click-to-dismiss backdrop.
///
/// The caller supplies the body as children, so the same shell serves both the
/// confirmation below and the "start task" form.
#[component]
pub fn Dialog(title: String, on_close: EventHandler<()>, children: Element) -> Element {
    let title_id = use_hook(|| format!("favetto-dialog-title-{}", next_dialog_id()));
    rsx! {
        div {
            class: "dialog-overlay",
            onclick: move |_| on_close.call(()),
            // Escape bubbles from the focused panel to the backdrop.
            onkeydown: move |event| {
                if event.key() == Key::Escape {
                    on_close.call(());
                }
            },
            div {
                class: "dialog",
                id: "{title_id}",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "{title_id}-heading",
                tabindex: "-1",
                // Move focus into the dialog so Escape and Tab work.
                onmounted: move |event| {
                    spawn(async move {
                        let _ = event.set_focus(true).await;
                    });
                },
                // Keep clicks inside the panel from reaching the backdrop.
                onclick: move |event| event.stop_propagation(),
                h2 { id: "{title_id}-heading", class: "dialog-title", "{title}" }
                div { class: "dialog-body", {children} }
            }
        }
    }
}

/// A yes/no confirmation with a busy state so the trigger stays single-shot.
#[component]
pub fn ConfirmDialog(
    title: String,
    message: String,
    confirm_label: String,
    /// Render the confirm button in the destructive style.
    #[props(default = false)]
    danger: bool,
    /// Disable both buttons while the confirmed request is in flight.
    #[props(default = false)]
    busy: bool,
    on_confirm: EventHandler<()>,
    on_close: EventHandler<()>,
) -> Element {
    let confirm_class = if danger { "btn danger" } else { "btn primary" };
    rsx! {
        Dialog { title, on_close,
            p { class: "dialog-message", "{message}" }
            div { class: "dialog-actions",
                button {
                    class: "btn",
                    disabled: busy,
                    onclick: move |_| on_close.call(()),
                    "Cancel"
                }
                button {
                    class: confirm_class,
                    disabled: busy,
                    onclick: move |_| on_confirm.call(()),
                    "{confirm_label}"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_dialog_mounts_host_side() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                ConfirmDialog {
                    title: "Cancel task".to_string(),
                    message: "This cannot be undone.".to_string(),
                    confirm_label: "Cancel task".to_string(),
                    danger: true,
                    busy: false,
                    on_confirm: move |_| {},
                    on_close: move |_| {},
                }
            }
        });
        dom.rebuild_in_place();
    }

    #[test]
    fn generic_dialog_accepts_children() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                Dialog { title: "Start task".to_string(), on_close: move |_| {},
                    p { "body" }
                }
            }
        });
        dom.rebuild_in_place();
    }
}
