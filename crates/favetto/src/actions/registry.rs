//! The built-in action registry.
//!
//! The curated built-ins are always registered; there is no config-driven action
//! yet (configurable/arbitrary actions are a deliberate, opt-in follow-up). The
//! registry is cheap and infallible to build.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::http::HttpRequestAction;
use super::Action;

/// Every built-in action, keyed by its registry id.
pub struct ActionRegistry {
    actions: BTreeMap<String, Arc<dyn Action>>,
}

impl Default for ActionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ActionRegistry {
    /// Build the registry seeded with every curated built-in.
    pub fn new() -> Self {
        let mut actions: BTreeMap<String, Arc<dyn Action>> = BTreeMap::new();
        let http: Arc<dyn Action> = Arc::new(HttpRequestAction);
        actions.insert(http.id().to_string(), http);
        Self { actions }
    }

    /// The action registered under `id`, if any.
    pub fn get(&self, id: &str) -> Option<Arc<dyn Action>> {
        self.actions.get(id).cloned()
    }

    /// Every registered id, sorted.
    pub fn ids(&self) -> Vec<&str> {
        self.actions.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_the_builtin_http_action() {
        let registry = ActionRegistry::new();
        assert_eq!(registry.ids(), vec!["http.request"]);
        assert!(registry.get("http.request").is_some());
        assert!(registry.get("mystery").is_none());
    }
}
