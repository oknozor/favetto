//! The active-repo switcher: a persistent nav control plus a searchable picker.
//!
//! Mirrors the TUI's Ctrl+P "Switch repo" popup: a leading `global — all repos`
//! row clears the active repo, then the known repos (from `repos.list`) filtered
//! case-insensitively on name or path, with an active marker and a
//! `(missing)` suffix for unavailable checkouts. Selecting a row calls
//! `repos.activate` and stores the returned registry, so the UI updates without
//! a reload.

use dioxus::prelude::*;
use favetto_wire::model::RepoInfo;
use favetto_wire::rpc::{EmptyParams, ReposActivateParams};

use crate::client::RpcClient;
use crate::components::dialog::Dialog;
use crate::components::icon::{AppIcon, AppIconKind};
use crate::state::{active_repo_label, AppState};

/// One row of the repo picker.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RepoOption {
    /// The repo to activate; `None` is the leading `global` row that clears it.
    pub name: Option<String>,
    /// The rendered label with a `(missing)` suffix for unavailable repos.
    pub label: String,
    /// Whether the row can be activated. Unavailable repos stay visible but
    /// inert.
    pub selectable: bool,
    /// Whether this row is the active repo (renders a marker icon).
    pub active: bool,
}

/// The picker rows: a leading `global` row plus the known repos matching
/// `filter` (a case-insensitive substring of the name or the path).
///
/// The `global` row is always present, matching the TUI picker.
pub fn repo_picker_entries(repos: &[RepoInfo], filter: &str) -> Vec<RepoOption> {
    let filter = filter.trim().to_lowercase();
    let matches = |haystack: &str| haystack.to_lowercase().contains(&filter);
    let mut entries = vec![RepoOption {
        name: None,
        label: "global  —  all repos".to_string(),
        selectable: true,
        active: false,
    }];
    for repo in repos {
        if !filter.is_empty() && !matches(&repo.name) && !matches(&repo.path) {
            continue;
        }
        let availability = if repo.available { "" } else { "  (missing)" };
        entries.push(RepoOption {
            name: Some(repo.name.clone()),
            label: format!("{}{availability}", repo.name),
            selectable: repo.available,
            active: repo.active,
        });
    }
    entries
}

/// Activate `name` (`None` clears it) and store the returned registry.
fn activate_repo(
    mut state: AppState,
    mut busy: Signal<bool>,
    mut error: Signal<Option<String>>,
    mut open: Signal<bool>,
    name: Option<String>,
) {
    busy.set(true);
    error.set(None);
    spawn(async move {
        let client = RpcClient::new((state.daemon_url)()).with_token((state.token)());
        match client.repos_activate(ReposActivateParams { name }).await {
            Ok(result) => {
                state.cache.write().set_repos(result.repos, result.active);
                busy.set(false);
                open.set(false);
            }
            Err(err) => {
                busy.set(false);
                error.set(Some(err.to_string()));
            }
        }
    });
}

/// The nav control: shows the active repo and opens the picker on click.
#[component]
pub fn RepoSwitcher() -> Element {
    let mut state = use_context::<AppState>();
    let mut open = use_signal(|| false);
    let mut filter = use_signal(String::new);
    let mut error = use_signal(|| Option::<String>::None);
    let busy = use_signal(|| false);

    // Fetch `repos.list` on mount and re-run whenever a `catalog.updated` push
    // bumps `catalog_version` (a repo add/remove/activate from another client).
    let _repos = use_resource(move || {
        let base = (state.daemon_url)();
        let token = (state.token)();
        let _ = (state.catalog_version)();
        async move {
            if let Ok(result) = RpcClient::new(base)
                .with_token(token)
                .repos_list(EmptyParams {})
                .await
            {
                state.cache.write().set_repos(result.repos, result.active);
            }
        }
    });

    let cache = (state.cache)();
    let label = active_repo_label(cache.active_repo.as_deref());
    let entries = repo_picker_entries(&cache.repos, &filter());
    let empty = !filter().trim().is_empty() && entries.len() == 1;

    rsx! {
        button {
            class: "repo-switcher",
            title: "Switch repository",
            onclick: move |_| {
                filter.set(String::new());
                error.set(None);
                open.set(true);
            },
            span { class: "repo-switcher-icon", AppIcon { kind: AppIconKind::Repo, class: "app-nav-icon" } }
            span { class: "repo-switcher-label", "{label}" }
        }
        if open() {
            Dialog { title: "Switch repository".to_string(), on_close: move |_| open.set(false),
                input {
                    class: "input repo-picker-search",
                    r#type: "search",
                    placeholder: "Search by name or path…",
                    aria_label: "Search repositories",
                    value: "{filter}",
                    oninput: move |event| filter.set(event.value()),
                }
                if empty {
                    p { class: "empty-state", "No repositories match." }
                }
                ul { class: "repo-picker-list",
                    for option in entries {
                        li { key: "{option.label}",
                            button {
                                class: if option.name.is_none() { "repo-option repo-option-global" } else { "repo-option" },
                                disabled: busy() || !option.selectable,
                                onclick: {
                                    let name = option.name.clone();
                                    move |_| activate_repo(state, busy, error, open, name.clone())
                                },
                                if option.active {
                                    AppIcon { kind: AppIconKind::ActiveRepo, class: "repo-option-marker" }
                                }
                                span { "{option.label}" }
                            }
                        }
                    }
                }
                if let Some(err) = error() {
                    p { class: "dialog-error", role: "alert", "{err}" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str, path: &str, available: bool, active: bool) -> RepoInfo {
        RepoInfo {
            name: name.to_string(),
            path: path.to_string(),
            available,
            active,
            source: favetto_wire::model::RepoSource::Manual,
        }
    }

    #[test]
    fn picker_prepends_a_global_row_and_marks_the_active_repo() {
        let repos = vec![
            repo("favetto", "/code/favetto", true, true),
            repo("web", "/code/web", true, false),
        ];
        let entries = repo_picker_entries(&repos, "");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, None);
        assert!(entries[0].selectable);
        assert!(!entries[0].active);
        assert_eq!(entries[1].name.as_deref(), Some("favetto"));
        assert!(entries[1].active);
        assert_eq!(entries[2].name.as_deref(), Some("web"));
        assert!(!entries[2].active);
        assert_eq!(entries[1].label, "favetto");
    }

    #[test]
    fn picker_marks_unavailable_repos_and_disables_them() {
        let repos = vec![repo("gone", "/code/gone", false, false)];
        let entries = repo_picker_entries(&repos, "");
        assert_eq!(entries[1].name.as_deref(), Some("gone"));
        assert!(entries[1].label.ends_with("(missing)"));
        assert!(!entries[1].selectable);
    }

    #[test]
    fn picker_filters_case_insensitively_on_name_or_path() {
        let repos = vec![
            repo("favetto", "/code/favetto", true, false),
            repo("other", "/code/web", true, false),
        ];
        let by_name = repo_picker_entries(&repos, "FAV");
        assert_eq!(by_name.len(), 2);
        assert_eq!(by_name[1].name.as_deref(), Some("favetto"));

        let by_path = repo_picker_entries(&repos, "web");
        assert_eq!(by_path.len(), 2);
        assert_eq!(by_path[1].name.as_deref(), Some("other"));

        let none = repo_picker_entries(&repos, "zzz");
        assert_eq!(none.len(), 1, "the global row always remains");
        assert_eq!(none[0].name, None);
    }
}
