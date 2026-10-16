//! Modal alert for a daemon-reported invalid config file.
//!
//! The daemon validates its on-disk config on connection (`config.validate`);
//! when it reports a parse/deserialization failure the host renders this dialog
//! with the resolved path and one line per error, until dismissed.

use dioxus::prelude::*;

use crate::components::dialog::Dialog;

/// A dismissible alert summarising a config parse failure: the path plus one
/// line per error.
#[component]
pub fn ConfigErrorDialog(path: String, errors: Vec<String>, on_close: EventHandler<()>) -> Element {
    rsx! {
        Dialog { title: "Invalid configuration".to_string(), on_close,
            p { class: "dialog-message",
                "The daemon could not parse its config file:"
            }
            p { class: "dialog-message", code { "{path}" } }
            for error in errors.iter() {
                p { class: "dialog-message", "{error}" }
            }
            div { class: "dialog-actions",
                button {
                    class: "btn primary",
                    onclick: move |_| on_close.call(()),
                    "Close"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_error_dialog_mounts_host_side() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                ConfigErrorDialog {
                    path: "/home/u/.config/favetto/config.toml".to_string(),
                    errors: vec![
                        "invalid type: string, expected a boolean for key `executor.parallel`"
                            .to_string(),
                    ],
                    on_close: move |_| {},
                }
            }
        });
        dom.rebuild_in_place();
    }
}
