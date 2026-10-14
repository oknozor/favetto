//! Catalog list and detail views (`/catalog`, `/catalog/:..name`).

use dioxus::prelude::*;
use favetto_wire::model::Task;
use favetto_wire::rpc::{CatalogEntry, CatalogGetParams, EmptyParams};

use crate::app::Route;
use crate::client::RpcClient;
use crate::components::markdown::{split_frontmatter, Markdown};
use crate::components::start_task::StartTaskDialog;
use crate::components::toast::{Toast, ToastKind};
use crate::state::AppState;

/// A one-line summary derived from the task prompt (the wire `CatalogEntry`
/// carries no description field).
pub fn description(entry: &CatalogEntry) -> String {
    entry
        .prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.trim_start_matches('#').trim())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

/// Searchable metadata chips derived from the entry (the wire type has no tags).
pub fn tags(entry: &CatalogEntry) -> Vec<String> {
    let mut tags = Vec::new();
    if let Some(repo) = &entry.repo {
        tags.push(format!("repo:{repo}"));
    }
    if let Some(agent) = &entry.agent {
        tags.push(format!("agent:{agent}"));
    }
    if let Some(provider) = &entry.provider {
        tags.push(format!("provider:{provider}"));
    }
    if let Some(model) = &entry.model {
        tags.push(format!("model:{model}"));
    }
    if entry.schedule.is_some() {
        tags.push("scheduled".into());
    }
    if entry.needs.is_some() {
        tags.push("pipeline".into());
    }
    for var in &entry.vars {
        tags.push(format!("var:{}", var.name));
    }
    tags
}

/// Whether an entry matches the search box (case-insensitive substring over the
/// name, the derived description, and the derived tags). An empty query matches.
pub fn matches_query(entry: &CatalogEntry, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    entry.name.to_lowercase().contains(&q)
        || description(entry).to_lowercase().contains(&q)
        || tags(entry)
            .iter()
            .any(|tag| tag.to_lowercase().contains(&q))
}

/// `/catalog` — the searchable list of catalog definitions.
#[component]
pub fn Catalog() -> Element {
    let state = use_context::<AppState>();
    let mut query = use_signal(String::new);
    // The catalog entry whose run dialog is open, plus the last start outcome.
    let mut start_for = use_signal(|| None::<CatalogEntry>);
    let mut toast = use_signal(|| Option::<(ToastKind, String)>::None);

    // Reading `catalog_version` inside the resource future makes Dioxus re-run
    // the fetch whenever a `catalog.updated` push bumps it (0.7 tracks signal
    // reads made while polling the resource).
    let resource = use_resource(move || async move {
        let base = (state.daemon_url)();
        let token = (state.token)();
        let _ = (state.catalog_version)();
        RpcClient::new(base)
            .with_token(token)
            .catalog_list(EmptyParams {})
            .await
    });

    let entries: Vec<CatalogEntry> = match &*resource.read_unchecked() {
        Some(Ok(list)) => list.clone(),
        _ => Vec::new(),
    };
    // Scope to the active repo, then the search box. A repo-local entry carries
    // its namespace in `repo`; global entries are unscoped.
    let cache = (state.cache)();
    let visible: Vec<CatalogEntry> = entries
        .iter()
        .filter(|entry| cache.repo_visible(entry.repo.as_deref()))
        .filter(|entry| matches_query(entry, &query()))
        .cloned()
        .collect();

    rsx! {
        main { id: "favetto-app", class: "catalog",
            h1 { "Catalog" }
            if let Some(repo) = &cache.active_repo {
                p { class: "catalog-scope", "Showing entries in repo: {repo}" }
            }
            input {
                class: "catalog-search",
                r#type: "search",
                placeholder: "Search by name or tag…",
                aria_label: "Search the catalog",
                value: "{query}",
                oninput: move |event| query.set(event.value()),
            }
            match &*resource.read_unchecked() {
                Some(Err(err)) => rsx! { p { class: "catalog-error", "Could not load the catalog: {err}" } },
                _ => rsx! {},
            }
            ul { class: "catalog-list",
                for entry in visible.iter() {
                    li { key: "{entry.name}", class: "catalog-item",
                        Link { to: Route::CatalogDetail { name: entry.name.split('/').map(str::to_string).collect() }, class: "catalog-link",
                            span { class: "catalog-name", "{entry.name}" }
                            if !description(entry).is_empty() {
                                span { class: "catalog-description", "{description(entry)}" }
                            }
                            span { class: "catalog-tags",
                                for tag in tags(entry) {
                                    span { class: "tag", "{tag}" }
                                }
                            }
                        }
                        button {
                            class: "btn catalog-run",
                            aria_label: "Run {entry.name}",
                            onclick: {
                                let entry = entry.clone();
                                move |_| start_for.set(Some(entry.clone()))
                            },
                            "Run"
                        }
                    }
                }
            }
            if let Some(entry) = start_for() {
                StartTaskDialog {
                    name: entry.name.clone(),
                    vars: entry.vars.clone(),
                    on_close: move |_| start_for.set(None),
                    on_started: move |_task: Task| {
                        start_for.set(None);
                        toast.set(Some((ToastKind::Success, "Task started.".to_string())));
                    },
                }
            }
            if let Some((kind, message)) = toast() {
                Toast {
                    kind,
                    message,
                    on_dismiss: move |_| toast.set(None),
                }
            }
        }
    }
}

/// `/catalog/:..name` — one definition with its Markdown body.
///
/// The name arrives as route segments (repo-qualified identities contain a `/`)
/// and is rejoined before it reaches `catalog.get`.
#[component]
pub fn CatalogDetail(name: Vec<String>) -> Element {
    let state = use_context::<AppState>();
    let name = name.join("/");
    let fetch_name = name.clone();
    let resource = use_resource(move || {
        let fetch_name = fetch_name.clone();
        async move {
            let base = (state.daemon_url)();
            let token = (state.token)();
            RpcClient::new(base)
                .with_token(token)
                .catalog_get(CatalogGetParams { name: fetch_name })
                .await
        }
    });

    // Own the markdown before building rsx so the read guard can drop.
    let loaded: Option<String> = match &*resource.read_unchecked() {
        Some(Ok(result)) => Some(result.markdown.clone()),
        _ => None,
    };

    rsx! {
        main { id: "favetto-app", class: "catalog-detail",
            Link { to: Route::Catalog {}, class: "back-link", "← Catalog" }
            h1 { "{name}" }
            match &*resource.read_unchecked() {
                None => rsx! { p { class: "loading", "Loading…" } },
                Some(Err(err)) => rsx! { p { class: "catalog-error", "Could not load '{name}': {err}" } },
                Some(Ok(_)) => {
                    let (header, body) = split_frontmatter(loaded.as_deref().unwrap_or_default());
                    rsx! {
                        if let Some(header) = header {
                            pre { class: "catalog-frontmatter", "{header}" }
                        }
                        Markdown { source: body.to_string() }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> CatalogEntry {
        serde_json::from_value(serde_json::json!({
            "name": "triage_issues",
            "agent": "claude",
            "provider": "anthropic",
            "model": "opus",
            "schedule": "0 9 * * *",
            "vars": [{ "name": "repo", "prompt": "Which repo?" }],
            "prompt": "# Triage issues\n\nFind and label them.\n",
        }))
        .unwrap()
    }

    #[test]
    fn description_uses_the_first_prompt_line() {
        assert_eq!(description(&entry()), "Triage issues");
    }

    #[test]
    fn tags_include_repo_agent_model_and_schedule() {
        let mut e = entry();
        e.repo = Some("favetto".into());
        e.needs = Some("plan".into());
        let tags = tags(&e);
        assert!(tags.contains(&"repo:favetto".to_string()));
        assert!(tags.contains(&"agent:claude".to_string()));
        assert!(tags.contains(&"provider:anthropic".to_string()));
        assert!(tags.contains(&"model:opus".to_string()));
        assert!(tags.contains(&"scheduled".to_string()));
        assert!(tags.contains(&"pipeline".to_string()));
        assert!(tags.contains(&"var:repo".to_string()));
    }

    #[test]
    fn matches_query_is_case_insensitive_over_name_and_tag() {
        let e = entry();
        assert!(matches_query(&e, "TRIAGE"));
        assert!(matches_query(&e, "agent:CLAUDE"));
        assert!(matches_query(&e, "ISSUES"));
        assert!(!matches_query(&e, "nonexistent"));
    }

    #[test]
    fn empty_query_matches_everything() {
        let e = entry();
        assert!(matches_query(&e, ""));
        assert!(matches_query(&e, "   "));
    }

    #[test]
    fn catalog_list_mounts_with_state() {
        let mut dom = VirtualDom::new(|| {
            use_context_provider(AppState::new);
            rsx! { Catalog {} }
        });
        dom.rebuild_in_place();
    }
}
