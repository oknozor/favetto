//! The Settings model: the connection draft, its test result and the logical
//! focus used by the keyboard.
//!
//! The endpoint normalization itself lives in [`crate::config`] beside the file
//! format, so the preview shown before saving and the value written to
//! `gpui.toml` can never disagree. This module is GPUI-free and unit-tested
//! without a window.

use crate::config::{self, EndpointError, DEFAULT_ENDPOINT};
use crate::text_input::TextInput;

/// One logical focus target inside Settings. The root owns the actual keyboard
/// focus, so this value only routes typed characters and draws the caret/ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsFocus {
    /// The daemon address field.
    Endpoint,
    /// The bearer token field (blank means "use the token file").
    Token,
    /// The theme choices.
    Theme,
    /// The density choices.
    Density,
    /// Test the draft connection.
    Test,
    /// Save the draft connection.
    Save,
    /// Reset the draft to the default endpoint and token file.
    Defaults,
}

impl SettingsFocus {
    /// Every focus target in Tab order.
    pub const ORDER: [Self; 7] = [
        Self::Endpoint,
        Self::Token,
        Self::Theme,
        Self::Density,
        Self::Test,
        Self::Save,
        Self::Defaults,
    ];

    /// Whether typed characters belong to this target.
    pub fn is_text(self) -> bool {
        matches!(self, Self::Endpoint | Self::Token)
    }

    fn cycle(self, delta: isize) -> Self {
        let index = Self::ORDER
            .iter()
            .position(|focus| *focus == self)
            .unwrap_or(0) as isize;
        Self::ORDER[(index + delta).rem_euclid(Self::ORDER.len() as isize) as usize]
    }
}

/// What Enter does on the focused control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsActivate {
    /// Cycle the theme preference.
    Theme,
    /// Toggle the density preference.
    Density,
    /// Run the connection test.
    Test,
    /// Save the draft connection.
    Save,
    /// Reset the draft to the default endpoint/token file.
    Defaults,
    /// Move to the next focus target (a text field cannot be "activated").
    Next,
}

/// The result of the last "Test connection".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionTest {
    /// No test has run for the current draft.
    Idle,
    /// A `system.ping` round trip is in flight.
    Running {
        /// The normalized endpoint being tested.
        endpoint: String,
    },
    /// The round trip succeeded.
    Ok {
        /// The normalized endpoint that answered.
        endpoint: String,
        /// Round-trip latency in milliseconds.
        latency_ms: u128,
        /// The daemon's working directory, from `system.ping`.
        cwd: String,
    },
    /// The round trip failed with the transport's exact error.
    Failed {
        /// The normalized endpoint that was tested.
        endpoint: String,
        /// The exact `ClientError` message.
        message: String,
    },
}

/// The editable Settings state.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsState {
    draft_endpoint: TextInput,
    draft_token: TextInput,
    focus: Option<SettingsFocus>,
    test: ConnectionTest,
    save_error: Option<String>,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self::new(DEFAULT_ENDPOINT, None)
    }
}

impl SettingsState {
    /// Seed the form from the loaded settings. Only a token that lives in
    /// `gpui.toml` is prefilled; a shared token-file value is never copied into
    /// the form (the source is shown under the field instead).
    pub fn new(endpoint: &str, token_override: Option<&str>) -> Self {
        Self {
            draft_endpoint: TextInput::new(endpoint.to_string()),
            draft_token: TextInput::new(token_override.unwrap_or_default().to_string()),
            focus: None,
            test: ConnectionTest::Idle,
            save_error: None,
        }
    }

    #[allow(dead_code)] // Read by the model's tests; the view paints the buffer.
    pub fn draft_endpoint(&self) -> &str {
        self.draft_endpoint.value()
    }

    pub fn draft_token(&self) -> &str {
        self.draft_token.value()
    }

    /// The endpoint (`true`) or token (`false`) buffer, regardless of which
    /// one currently has focus. Formatted/rendered fields keep their value when
    /// the other field is focused; `is_editing_text` gates editing.
    pub fn text_input(&self, endpoint: bool) -> Option<&TextInput> {
        Some(if endpoint {
            &self.draft_endpoint
        } else {
            &self.draft_token
        })
    }

    pub fn text_input_mut(&mut self, endpoint: bool) -> Option<&mut TextInput> {
        Some(if endpoint {
            &mut self.draft_endpoint
        } else {
            &mut self.draft_token
        })
    }

    /// Clear the test result and save error after an in-place edit.
    pub fn after_edit(&mut self) {
        self.test = ConnectionTest::Idle;
        self.save_error = None;
    }

    pub fn focus(&self) -> Option<SettingsFocus> {
        self.focus
    }

    pub fn is_editing_text(&self) -> bool {
        self.focus.is_some_and(SettingsFocus::is_text)
    }

    pub fn set_focus(&mut self, focus: Option<SettingsFocus>) {
        self.focus = focus;
    }

    pub fn blur(&mut self) {
        self.focus = None;
    }

    /// Tab / Shift+Tab. From "nothing focused" Tab starts at the first field
    /// and Shift+Tab starts at the last action.
    pub fn cycle_focus(&mut self, delta: isize) {
        self.focus = Some(match self.focus {
            Some(focus) => focus.cycle(delta),
            None if delta > 0 => SettingsFocus::ORDER[0],
            None => *SettingsFocus::ORDER
                .last()
                .unwrap_or(&SettingsFocus::Defaults),
        });
    }

    /// Append a typed character to the focused text field.
    #[allow(dead_code)] // Exercised by the model's tests.
    pub fn push_char(&mut self, character: char) {
        if character.is_control() {
            return;
        }
        match self.focus {
            Some(SettingsFocus::Endpoint) => {
                self.draft_endpoint.push_char(character);
                self.after_edit();
            }
            Some(SettingsFocus::Token) => {
                self.draft_token.push_char(character);
                self.after_edit();
            }
            _ => {}
        }
    }

    /// Remove the last character of the focused text field.
    #[allow(dead_code)] // Exercised by the model's tests.
    pub fn backspace(&mut self) {
        match self.focus {
            Some(SettingsFocus::Endpoint) => {
                self.draft_endpoint.backspace();
                self.after_edit();
            }
            Some(SettingsFocus::Token) => {
                self.draft_token.backspace();
                self.after_edit();
            }
            _ => {}
        }
    }

    /// The normalized endpoint the draft would save, or the precise reason it
    /// cannot be saved.
    pub fn normalized(&self) -> Result<String, EndpointError> {
        config::validate_endpoint(self.draft_endpoint.value())
    }

    /// The token to send for a test/save: the typed one, otherwise the caller's
    /// fallback (the token currently in effect).
    pub fn token_or(&self, fallback: Option<&str>) -> Option<String> {
        let typed = self.draft_token.value().trim();
        if typed.is_empty() {
            fallback.map(str::to_string)
        } else {
            Some(typed.to_string())
        }
    }

    /// What Enter does for the current focus.
    pub fn activate(&self) -> SettingsActivate {
        match self.focus {
            Some(SettingsFocus::Theme) => SettingsActivate::Theme,
            Some(SettingsFocus::Density) => SettingsActivate::Density,
            Some(SettingsFocus::Test) => SettingsActivate::Test,
            Some(SettingsFocus::Save) => SettingsActivate::Save,
            Some(SettingsFocus::Defaults) => SettingsActivate::Defaults,
            Some(SettingsFocus::Endpoint) | Some(SettingsFocus::Token) | None => {
                SettingsActivate::Next
            }
        }
    }

    pub fn set_draft(&mut self, endpoint: &str, token: Option<&str>) {
        self.draft_endpoint = TextInput::new(endpoint.to_string());
        self.draft_token = TextInput::new(token.unwrap_or_default().to_string());
        self.test = ConnectionTest::Idle;
        self.save_error = None;
    }

    /// "Use defaults": the local endpoint plus the shared token file.
    pub fn reset_to_defaults(&mut self) {
        self.set_draft(DEFAULT_ENDPOINT, None);
        self.focus = Some(SettingsFocus::Endpoint);
    }

    pub fn test(&self) -> &ConnectionTest {
        &self.test
    }

    pub fn begin_test(&mut self, endpoint: String) {
        self.save_error = None;
        self.test = ConnectionTest::Running { endpoint };
    }

    pub fn finish_test_ok(&mut self, endpoint: String, latency_ms: u128, cwd: String) {
        if self.testing() == Some(endpoint.as_str()) {
            self.test = ConnectionTest::Ok {
                endpoint,
                latency_ms,
                cwd,
            };
        }
    }

    pub fn finish_test_failed(&mut self, endpoint: String, message: String) {
        if self.testing() == Some(endpoint.as_str()) {
            self.test = ConnectionTest::Failed { endpoint, message };
        }
    }

    fn testing(&self) -> Option<&str> {
        match &self.test {
            ConnectionTest::Running { endpoint } => Some(endpoint),
            _ => None,
        }
    }

    pub fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
    }

    pub fn set_save_error(&mut self, message: Option<String>) {
        self.save_error = message;
    }
}

/// `3 ms`, `1.2 s` — the connection-test latency in the form the user reads.
pub fn latency_label(latency_ms: u128) -> String {
    if latency_ms < 1_000 {
        format!("{latency_ms} ms")
    } else {
        format!("{:.1} s", latency_ms as f64 / 1_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_edits_only_the_focused_text_field() {
        let mut settings = SettingsState::new("127.0.0.1:7878", None);
        settings.push_char('x');
        assert_eq!(settings.draft_endpoint(), "127.0.0.1:7878");

        settings.set_focus(Some(SettingsFocus::Endpoint));
        for character in "/rpc".chars() {
            settings.push_char(character);
        }
        assert_eq!(settings.draft_endpoint(), "127.0.0.1:7878/rpc");
        settings.backspace();
        settings.backspace();
        settings.backspace();
        settings.backspace();
        assert_eq!(settings.draft_endpoint(), "127.0.0.1:7878");

        settings.set_focus(Some(SettingsFocus::Token));
        settings.push_char('t');
        assert_eq!(settings.draft_token(), "t");
    }

    #[test]
    fn tab_cycles_every_control_and_wraps() {
        let mut settings = SettingsState::default();
        settings.cycle_focus(1);
        assert_eq!(settings.focus(), Some(SettingsFocus::Endpoint));
        for _ in 0..SettingsFocus::ORDER.len() - 1 {
            settings.cycle_focus(1);
        }
        assert_eq!(settings.focus(), Some(SettingsFocus::Defaults));
        settings.cycle_focus(1);
        assert_eq!(settings.focus(), Some(SettingsFocus::Endpoint));
        settings.cycle_focus(-1);
        assert_eq!(settings.focus(), Some(SettingsFocus::Defaults));
    }

    #[test]
    fn a_blank_token_falls_back_to_the_current_token() {
        let mut settings = SettingsState::new("127.0.0.1:7878", None);
        assert_eq!(
            settings.token_or(Some("file-token")),
            Some("file-token".into())
        );
        settings.set_focus(Some(SettingsFocus::Token));
        settings.push_char('o');
        settings.push_char('v');
        assert_eq!(settings.token_or(Some("file-token")), Some("ov".into()));
        settings.set_draft("127.0.0.1:7878", None);
        assert_eq!(settings.token_or(None), None);
    }

    #[test]
    fn the_preview_uses_the_precise_endpoint_error() {
        let mut settings = SettingsState::default();
        settings.set_draft("ftp://example.com", None);
        assert!(matches!(
            settings.normalized(),
            Err(EndpointError::UnsupportedScheme(_))
        ));
        settings.set_draft("127.0.0.1:7878/rpc", None);
        assert_eq!(
            settings.normalized(),
            Ok("http://127.0.0.1:7878".to_string())
        );
    }

    #[test]
    fn a_stale_test_result_is_ignored() {
        let mut settings = SettingsState::default();
        settings.begin_test("http://old".to_string());
        settings.finish_test_ok("http://new".to_string(), 4, "/tmp".to_string());
        assert_eq!(
            settings.test(),
            &ConnectionTest::Running {
                endpoint: "http://old".to_string()
            }
        );
        settings.finish_test_failed("http://old".to_string(), "network: refused".to_string());
        assert!(matches!(
            settings.test(),
            ConnectionTest::Failed { message, .. } if message.contains("refused")
        ));
    }
}
