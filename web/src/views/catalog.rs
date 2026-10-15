//! Catalog list and detail views (`/catalog`, `/catalog/:..name`).

use dioxus::prelude::*;
use favetto_wire::model::Task;
use favetto_wire::rpc::{CatalogEntry, CatalogGetParams, EmptyParams};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use crate::app::Route;
use crate::client::RpcClient;
use crate::components::icon::{AppIcon, AppIconKind};
use crate::components::markdown::{split_frontmatter, Markdown};
use crate::components::start_task::StartTaskDialog;
use crate::components::toast::{Toast, ToastKind};
use crate::state::{AppState, Cache};

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
    // `scheduled` is shown as a kind badge on task rows, but the catalog list
    // keeps it as a metadata chip (the schedule is a property of the entry).
    if entry.schedule.is_some() {
        tags.push("scheduled".into());
    }
    tags.extend(task_tags(entry));
    tags
}

/// The catalog-derived kind badge label shown on a task row: `action`,
/// `scheduled`, or `agent`.
///
/// `scheduled` is a trigger rather than a mutually-exclusive kind, so the
/// precedence is `action` > `scheduled` > `agent`: an action task is a distinct
/// execution kind, while a scheduled task is usually also an agent task. The
/// label is definition-based (derived from the catalog), not per-run.
pub fn task_kind(entry: &CatalogEntry) -> &'static str {
    if entry.kind == "action" {
        "action"
    } else if entry.schedule.is_some() {
        "scheduled"
    } else {
        "agent"
    }
}

/// The metadata chips shared with the task list, excluding the repo tag and the
/// kind label: a task row renders the repo as its own chip and the kind as its
/// own badge next to the title.
pub fn task_tags(entry: &CatalogEntry) -> Vec<String> {
    let mut tags = Vec::new();
    if let Some(agent) = &entry.agent {
        tags.push(format!("agent:{agent}"));
    }
    if let Some(provider) = &entry.provider {
        tags.push(format!("provider:{provider}"));
    }
    if let Some(model) = &entry.model {
        tags.push(format!("model:{model}"));
    }
    if entry.needs.is_some() {
        tags.push("pipeline".into());
    }
    for var in &entry.vars {
        tags.push(format!("var:{}", var.name));
    }
    tags
}

/// Field weights used to rank a match. A hit in the task name outranks one in a
/// metadata field, which in turn outranks an incidental hit in the (long) prompt
/// body, so named tasks surface above body-only matches.
const NAME_WEIGHT: i64 = 32;
const VAR_NAME_WEIGHT: i64 = 16;
const META_WEIGHT: i64 = 4;
const PROMPT_WEIGHT: i64 = 1;

/// Every searchable `(field text, weight)` pair for an entry: the task name,
/// the metadata fields (`kind`, `agent`, `provider`, `model`, `cwd`, `schedule`,
/// `needs`, `repo`), the derived tag chips, each variable's name and prompt, and
/// finally the full prompt body.
fn searchable_fields(entry: &CatalogEntry) -> Vec<(String, i64)> {
    let mut fields = vec![(entry.name.clone(), NAME_WEIGHT)];
    fields.push((entry.kind.clone(), META_WEIGHT));
    for value in [
        &entry.agent,
        &entry.provider,
        &entry.model,
        &entry.cwd,
        &entry.schedule,
        &entry.needs,
        &entry.repo,
    ]
    .into_iter()
    .flatten()
    {
        fields.push((value.clone(), META_WEIGHT));
    }
    for var in &entry.vars {
        fields.push((var.name.clone(), VAR_NAME_WEIGHT));
        fields.push((var.prompt.clone(), META_WEIGHT));
    }
    for tag in tags(entry) {
        fields.push((tag, META_WEIGHT));
    }
    fields.push((entry.prompt.clone(), PROMPT_WEIGHT));
    fields
}

/// The total weighted score for an entry against already-tokenized query terms,
/// or `None` when a term matches no field (every term must match somewhere).
fn score_entry(matcher: &SkimMatcherV2, entry: &CatalogEntry, tokens: &[&str]) -> Option<i64> {
    let fields = searchable_fields(entry);
    let mut total = 0i64;
    for token in tokens {
        let mut best: Option<i64> = None;
        for (text, weight) in &fields {
            if let Some(score) = matcher.fuzzy_match(text, token) {
                let weighted = score.saturating_mul(*weight);
                best = Some(best.map_or(weighted, |b| b.max(weighted)));
            }
        }
        total = total.saturating_add(best?);
    }
    Some(total)
}

/// Full-text, fuzzy, ranked search of `entries` against `query`.
///
/// Matching is case-insensitive and token-wise: every whitespace-separated term
/// must match some field (fuzzy subsequence, so typos and abbreviations work),
/// and an entry's score is the sum of its best weighted field hits. Results are
/// ordered by descending score; ties keep the incoming catalog order. An empty
/// or whitespace-only query returns `entries` unchanged.
pub fn search(entries: &[CatalogEntry], query: &str) -> Vec<CatalogEntry> {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() {
        return entries.to_vec();
    }
    let matcher = SkimMatcherV2::default().ignore_case();
    let mut scored: Vec<(i64, CatalogEntry)> = entries
        .iter()
        .filter_map(|entry| score_entry(&matcher, entry, &tokens).map(|s| (s, entry.clone())))
        .collect();
    // `sort_by_key` is stable, so equal scores preserve the incoming order.
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, entry)| entry).collect()
}

/// Repo-scope `entries` for the active repository (via [`Cache::repo_visible`])
/// and rank the survivors against `query` with [`search`].
pub fn search_visible(cache: &Cache, entries: &[CatalogEntry], query: &str) -> Vec<CatalogEntry> {
    let scoped: Vec<CatalogEntry> = entries
        .iter()
        .filter(|entry| cache.repo_visible(entry.repo.as_deref()))
        .cloned()
        .collect();
    search(&scoped, query)
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
    // Scope to the active repo, then run the full-text search. A repo-local
    // entry carries its namespace in `repo`; global entries are unscoped.
    let cache = (state.cache)();
    let query_value = query();
    let visible = search_visible(&cache, &entries, &query_value);
    let count_label = if query_value.trim().is_empty() {
        format!("{} entries", visible.len())
    } else {
        format!("{} matches", visible.len())
    };

    rsx! {
        main { id: "favetto-app", class: "catalog",
            h1 { "Catalog" }
            if let Some(repo) = &cache.active_repo {
                p { class: "catalog-scope", "Showing entries in repo: {repo}" }
            }
            input {
                class: "catalog-search",
                r#type: "search",
                placeholder: "Search tasks, prompts, agents, models…",
                aria_label: "Search the catalog",
                value: "{query}",
                oninput: move |event| query.set(event.value()),
            }
            p { class: "catalog-count", "{count_label}" }
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
            Link { to: Route::Catalog {}, class: "back-link",
                AppIcon { kind: AppIconKind::Back, class: "icon-inline" }
                " Catalog"
            }
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
    fn task_tags_keep_metadata_and_drop_the_repo() {
        let mut e = entry();
        e.repo = Some("favetto".into());
        e.needs = Some("plan".into());
        let tags = task_tags(&e);
        assert!(!tags.iter().any(|tag| tag.starts_with("repo:")));
        assert!(tags.contains(&"agent:claude".to_string()));
        assert!(tags.contains(&"provider:anthropic".to_string()));
        assert!(tags.contains(&"model:opus".to_string()));
        // `scheduled` is a kind badge on task rows, not a metadata chip.
        assert!(!tags.contains(&"scheduled".to_string()));
        assert!(tags.contains(&"pipeline".to_string()));
        assert!(tags.contains(&"var:repo".to_string()));
    }

    #[test]
    fn task_kind_prefers_action_then_scheduled_then_agent() {
        // An action task is always `action`, scheduled or not.
        let mut action = entry();
        action.kind = "action".into();
        assert_eq!(task_kind(&action), "action");
        assert_eq!(task_kind(&custom("t", "")), "agent");
    }

    #[test]
    fn task_kind_shows_scheduled_for_a_cron_agent_task() {
        // `entry()` carries a schedule but defaults to the agent kind.
        assert_eq!(entry().schedule.as_deref(), Some("0 9 * * *"));
        assert_eq!(task_kind(&entry()), "scheduled");

        // Dropping the schedule falls back to the default kind.
        let mut plain = entry();
        plain.schedule = None;
        assert_eq!(task_kind(&plain), "agent");
    }

    fn custom(name: &str, prompt: &str) -> CatalogEntry {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "vars": [],
            "prompt": prompt,
        }))
        .unwrap()
    }

    fn names(entries: &[CatalogEntry]) -> Vec<String> {
        entries.iter().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn search_is_case_insensitive_and_fuzzy_over_the_name() {
        let e = entry();
        for query in ["TRIAGE", "trie", "trige", "agent:CLAUDE", "AGENT:claude"] {
            assert_eq!(
                names(&search(std::slice::from_ref(&e), query)),
                vec!["triage_issues"],
                "{query}"
            );
        }
        assert!(search(&[e], "nonexistent").is_empty());
    }

    #[test]
    fn search_matches_the_full_prompt_body() {
        let e = entry();
        // "label" only appears past the first prompt line, which the old
        // substring filter never reached.
        assert_eq!(
            names(&search(std::slice::from_ref(&e), "label")),
            vec!["triage_issues"]
        );
        assert_eq!(
            names(&search(std::slice::from_ref(&e), "Find and label")),
            vec!["triage_issues"]
        );
        assert!(search(&[e], "unrelated").is_empty());
    }

    #[test]
    fn search_covers_every_catalog_entry_field() {
        let mut e = entry();
        e.cwd = Some("/srv/work".into());
        e.needs = Some("plan".into());
        e.repo = Some("favetto".into());
        for query in [
            "triage_issues", // name
            "agent",         // kind (default)
            "claude",        // agent
            "anthropic",     // provider
            "opus",          // model
            "/srv",          // cwd
            "0 9",           // schedule
            "plan",          // needs
            "repo",          // var name
            "Which repo",    // var prompt
            "favetto",       // repo
            "scheduled",     // derived tag
            "pipeline",      // derived tag
            "label",         // prompt body
        ] {
            assert_eq!(
                names(&search(&[e.clone()], query)),
                vec!["triage_issues"],
                "{query}"
            );
        }

        // The `kind` value itself is searchable, independent of the agent tag.
        let mut action = e.clone();
        action.kind = "action".into();
        action.agent = None;
        assert_eq!(names(&search(&[action], "action")), vec!["triage_issues"]);
    }

    #[test]
    fn search_matches_multi_token_queries() {
        let mut e = entry();
        e.repo = Some("favetto".into());
        assert_eq!(
            names(&search(&[e.clone()], "claude opus")),
            vec!["triage_issues"]
        );
        assert_eq!(
            names(&search(&[e], "triage favetto")),
            vec!["triage_issues"]
        );
    }

    #[test]
    fn search_ranks_name_matches_above_body_matches() {
        let name_hit = custom("triage_issues", "Do the work.\n");
        let body_hit = custom("run_report", "Triage the inbox then summarize.\n");
        // Input order is worst-first; the name match must sort ahead.
        let results = search(&[body_hit, name_hit], "triage");
        assert_eq!(names(&results), vec!["triage_issues", "run_report"]);
    }

    #[test]
    fn empty_query_preserves_catalog_order() {
        let first = custom("run_report", "Triage the inbox.\n");
        let second = custom("triage_issues", "Find and label them.\n");
        for query in ["", "   "] {
            assert_eq!(
                names(&search(&[first.clone(), second.clone()], query)),
                vec!["run_report", "triage_issues"],
                "{query:?}"
            );
        }
    }

    #[test]
    fn search_respects_repo_scoping() {
        let global = custom("global_task", "Global.\n");
        let mut local = custom("local_task", "Local.\n");
        local.repo = Some("favetto".into());
        let mut other = custom("other_task", "Other.\n");
        other.repo = Some("other".into());

        let mut cache = Cache {
            active_repo: Some("favetto".into()),
            ..Default::default()
        };
        let scoped = search_visible(&cache, &[global.clone(), local, other.clone()], "");
        assert_eq!(names(&scoped), vec!["local_task"]);

        // With no active repo the global view returns everything, in order.
        cache.active_repo = None;
        let all = search_visible(&cache, &[global, other], "");
        assert_eq!(names(&all), vec!["global_task", "other_task"]);
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
