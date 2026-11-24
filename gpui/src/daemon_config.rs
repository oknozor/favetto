//! The daemon's global `config.toml` draft: raw TOML, revision and apply state.
//!
//! The editor is deliberately a raw TOML surface. Every section of
//! `FavettoConfig` is reachable without duplicating the field list in the client,
//! and comments/unknown keys survive because the daemon writes the draft with
//! `toml_edit` instead of reserializing the struct.
//!
//! This module is GPUI-free and unit-tested without a window.

use favetto_wire::rpc::{ConfigGetResult, ConfigOverride, ConfigSetResult};

use crate::text_input::TextInput;

/// What the daemon reported after the last load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonConfigState {
    draft: TextInput,
    /// The raw TOML the daemon last returned, used by Reload.
    snapshot: Option<String>,
    path: Option<String>,
    revision: Option<String>,
    exists: bool,
    loading: bool,
    saving: bool,
    errors: Vec<String>,
    warnings: Vec<String>,
    overrides: Vec<ConfigOverride>,
    restart_required: Vec<String>,
    live_sections: Vec<String>,
    loaded: bool,
    load_error: Option<String>,
}

impl DaemonConfigState {
    pub fn draft(&self) -> &TextInput {
        &self.draft
    }

    pub fn draft_mut(&mut self) -> &mut TextInput {
        &mut self.draft
    }

    pub fn insert_newline(&mut self) {
        self.draft.insert("\n");
        self.after_edit();
    }

    /// Mark the draft edited so a subsequent load cannot clobber it.
    pub fn after_edit(&mut self) {
        self.loaded = true;
        self.errors.clear();
        self.live_sections.clear();
    }

    pub fn begin_load(&mut self) {
        self.loading = true;
        self.load_error = None;
    }

    pub fn fail_load(&mut self, message: String) {
        self.loading = false;
        self.load_error = Some(message);
    }

    /// Fold a `config.get` result. A dirty draft is never overwritten; the path,
    /// revision and diagnostics still refresh.
    pub fn apply_get(&mut self, result: &ConfigGetResult) {
        // Decide whether the draft may be replaced before the new snapshot is
        // recorded; comparing against it afterwards would always look dirty.
        let clean = !self.is_dirty();
        self.loading = false;
        self.load_error = None;
        self.loaded = true;
        self.path = Some(result.path.clone());
        self.revision = Some(result.revision.clone());
        self.exists = result.exists;
        if clean {
            self.draft.set_value(result.raw.clone());
        }
        self.snapshot = Some(result.raw.clone());
        self.errors = result.parse_errors.clone();
        self.warnings = result.warnings.clone();
        self.overrides = result.overrides.clone();
        self.restart_required = result.restart_required.clone();
        self.live_sections.clear();
    }

    pub fn begin_save(&mut self) {
        self.saving = true;
        self.errors.clear();
    }

    pub fn fail_save(&mut self, message: String) {
        self.saving = false;
        self.errors = vec![message];
    }

    /// Fold a `config.set` result. A rejected save keeps the draft and the
    /// daemon's current revision so the next attempt is guarded correctly.
    pub fn apply_set(&mut self, result: &ConfigSetResult) {
        self.saving = false;
        self.path = Some(result.path.clone());
        self.revision = Some(result.revision.clone());
        self.errors = result.errors.clone();
        self.warnings = result.warnings.clone();
        self.restart_required = result.restart_required.clone();
        self.live_sections = result.live_sections.clone();
        if result.saved {
            self.snapshot = Some(self.draft.value().to_string());
            self.exists = true;
        }
    }

    /// Discard edits and restore the last loaded snapshot.
    pub fn reset(&mut self) {
        if let Some(snapshot) = &self.snapshot {
            self.draft.set_value(snapshot.clone());
        }
        self.errors.clear();
        self.live_sections.clear();
    }

    pub fn raw(&self) -> &str {
        self.draft.value()
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    pub fn revision(&self) -> Option<&str> {
        self.revision.as_deref()
    }

    pub fn exists(&self) -> bool {
        self.exists
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn is_saving(&self) -> bool {
        self.saving
    }

    pub fn has_loaded(&self) -> bool {
        self.loaded
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn overrides(&self) -> &[ConfigOverride] {
        &self.overrides
    }

    pub fn restart_required(&self) -> &[String] {
        &self.restart_required
    }

    pub fn live_sections(&self) -> &[String] {
        &self.live_sections
    }

    /// Dirty against the last snapshot the daemon returned.
    pub fn is_dirty(&self) -> bool {
        self.snapshot
            .as_deref()
            .is_some_and(|snapshot| snapshot != self.draft.value())
    }

    /// The save action is available for a loaded, edited, idle draft.
    pub fn can_save(&self) -> bool {
        self.loaded && !self.saving && !self.loading && self.is_dirty()
    }

    pub fn status(&self) -> DaemonConfigStatus {
        if self.saving {
            DaemonConfigStatus::Saving
        } else if self.loading {
            DaemonConfigStatus::Loading
        } else if !self.errors.is_empty() {
            DaemonConfigStatus::Invalid
        } else if self.is_dirty() {
            DaemonConfigStatus::Dirty
        } else if self.loaded {
            DaemonConfigStatus::Saved
        } else {
            DaemonConfigStatus::Idle
        }
    }
}

/// The one-line status shown beside the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonConfigStatus {
    Idle,
    Loading,
    Saving,
    Dirty,
    Invalid,
    Saved,
}

impl DaemonConfigStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Not loaded",
            Self::Loading => "Loading…",
            Self::Saving => "Saving…",
            Self::Dirty => "Unsaved changes",
            Self::Invalid => "Invalid draft",
            Self::Saved => "Valid",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(raw: &str, revision: &str) -> ConfigGetResult {
        ConfigGetResult {
            path: "/home/u/.config/favetto/config.toml".to_string(),
            raw: raw.to_string(),
            revision: revision.to_string(),
            exists: true,
            parse_errors: Vec::new(),
            warnings: Vec::new(),
            overrides: Vec::new(),
            restart_required: Vec::new(),
            effective: None,
        }
    }

    #[test]
    fn a_fresh_snapshot_seeds_the_draft_and_stays_clean() {
        let mut state = DaemonConfigState::default();
        state.begin_load();
        state.apply_get(&snapshot("[daemon]\n", "r1"));
        assert_eq!(state.raw(), "[daemon]\n");
        assert_eq!(state.revision(), Some("r1"));
        assert!(!state.is_dirty());
        assert_eq!(state.status(), DaemonConfigStatus::Saved);
    }

    #[test]
    fn edits_are_dirty_and_are_not_clobbered_by_a_background_load() {
        let mut state = DaemonConfigState::default();
        state.apply_get(&snapshot("[daemon]\n", "r1"));
        state.draft_mut().insert("listen = \"127.0.0.1:1\"\n");
        state.after_edit();
        assert!(state.is_dirty());
        assert_eq!(state.status(), DaemonConfigStatus::Dirty);

        state.apply_get(&snapshot("[daemon]\nlisten = \"old\"\n", "r2"));
        assert!(state.raw().contains("127.0.0.1:1"));
        // The revision still tracks the daemon for the save guard.
        assert_eq!(state.revision(), Some("r2"));
    }

    #[test]
    fn a_rejected_save_keeps_the_draft_and_reports_errors() {
        let mut state = DaemonConfigState::default();
        state.apply_get(&snapshot("[daemon]\n", "r1"));
        state.draft_mut().insert("listen = 42\n");
        state.after_edit();
        state.begin_save();
        assert_eq!(state.status(), DaemonConfigStatus::Saving);
        state.apply_set(&ConfigSetResult {
            saved: false,
            path: "/home/u/.config/favetto/config.toml".to_string(),
            revision: "r1".to_string(),
            errors: vec!["invalid type: integer".to_string()],
            warnings: Vec::new(),
            changed_sections: Vec::new(),
            restart_required: Vec::new(),
            live_sections: Vec::new(),
        });
        assert!(state.is_dirty());
        assert_eq!(state.errors().len(), 1);
        assert_eq!(state.status(), DaemonConfigStatus::Invalid);
        assert_eq!(state.revision(), Some("r1"));
    }

    #[test]
    fn a_successful_save_clears_dirty_and_records_restart_sections() {
        let mut state = DaemonConfigState::default();
        state.apply_get(&snapshot("[daemon]\n", "r1"));
        state.draft_mut().insert("listen = \"127.0.0.1:9999\"\n");
        state.after_edit();
        state.apply_set(&ConfigSetResult {
            saved: true,
            path: "/home/u/.config/favetto/config.toml".to_string(),
            revision: "r2".to_string(),
            errors: Vec::new(),
            warnings: Vec::new(),
            changed_sections: vec!["daemon".to_string()],
            restart_required: vec!["daemon".to_string()],
            live_sections: Vec::new(),
        });
        assert!(!state.is_dirty());
        assert_eq!(state.revision(), Some("r2"));
        assert_eq!(state.restart_required(), ["daemon"]);
        assert_eq!(state.status(), DaemonConfigStatus::Saved);
    }

    #[test]
    fn reset_restores_the_last_snapshot() {
        let mut state = DaemonConfigState::default();
        state.apply_get(&snapshot("[daemon]\n", "r1"));
        state.draft_mut().insert("junk");
        state.after_edit();
        state.reset();
        assert_eq!(state.raw(), "[daemon]\n");
        assert!(!state.is_dirty());
    }

    #[test]
    fn newlines_are_inserted_and_count_as_an_edit() {
        let mut state = DaemonConfigState::default();
        state.apply_get(&snapshot("[daemon]\n", "r1"));
        state.insert_newline();
        assert!(state.raw().ends_with("\n\n"));
        assert!(state.is_dirty());
    }
}
