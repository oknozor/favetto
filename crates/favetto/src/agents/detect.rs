//! Pure heuristics for spotting an agent that is blocked on user input.
//!
//! The agent-specific matchers (opencode, Claude, …) are deliberately narrow and
//! pinned by unit tests; [`generic_awaiting_input`] is a conservative fallback
//! used by [`super::AgentManager`] only after the PTY has been quiet for a while.

use favetto_core::model::{AwaitingInputKind, AwaitingInputReason};

/// The last `n` non-empty visible lines, newest last.
pub(crate) fn prompt_lines(text: &str, n: usize) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() > n {
        lines.drain(..lines.len() - n);
    }
    lines
}

/// Whether a line looks like a numbered/selectable menu entry (`1)`, `2.`, `> 3:`).
fn numbered_option(line: &str) -> bool {
    let trimmed = line.trim_start_matches(['>', '❯', '▶', ' ', '*', '-', '|']);
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return false;
    }
    matches!(
        trimmed[digits.len()..].chars().next(),
        Some(')') | Some('.') | Some(':')
    )
}

/// Explicit yes/no confirmation tokens. Matched case-insensitively anywhere in a
/// line; these are unambiguous dialog evidence.
const CONFIRM_MARKERS: &[&str] = &[
    "[y/n]",
    "(y/n)",
    "[Y/n]",
    "[yes/no]",
    "(yes/no)",
    "yes/no",
    "press enter",
    "press any key",
    "press a key",
    "esc to cancel",
];

/// Words/phrases that turn an interrogative or colon-terminated line into a
/// prompt. Deliberately NOT matched on their own (that was the false-positive
/// source).
const PROMPT_VERBS: &[&str] = &[
    "permission",
    "allow",
    "deny",
    "reject",
    "approve",
    "passphrase",
    "pinentry",
    "password",
    "enter pass",
    "enter your",
    "do you want",
    "are you sure",
    "proceed",
    "continue",
    "select an option",
    "choose an option",
    "confirm",
];

/// The shared "does this look like a prompt" gate. Requires real dialog evidence:
/// an explicit confirmation token, a prompt verb on a question/colon-terminated
/// line, or two or more numbered options. Bare composer glyphs, any `?`, and any
/// line ending in `:` are no longer enough on their own.
pub(crate) fn has_prompt_marker(lines: &[String]) -> bool {
    // A real menu / choice: two or more numbered options.
    if lines.iter().filter(|line| numbered_option(line)).count() >= 2 {
        return true;
    }
    lines.iter().any(|line| {
        let lower = line.to_lowercase();
        // Explicit confirmation tokens anywhere.
        CONFIRM_MARKERS.iter().any(|marker| lower.contains(marker))
            // A prompt verb only on a question or a colon-terminated line.
            || ((lower.contains('?') || line.ends_with(':'))
                && PROMPT_VERBS.iter().any(|verb| lower.contains(verb)))
    })
}

fn reason(kind: AwaitingInputKind, lines: &[String]) -> AwaitingInputReason {
    let message = lines
        .last()
        .map(|line| line.chars().take(200).collect::<String>())
        .unwrap_or_default();
    AwaitingInputReason {
        kind,
        message,
        request_id: None,
        options: Vec::new(),
        allow_always: false,
    }
}

/// Classify a tail that has already passed the marker gate.
fn classify(lines: &[String]) -> AwaitingInputReason {
    let joined = lines.join("\n").to_lowercase();
    if joined.contains("passphrase")
        || joined.contains("pinentry")
        || joined.contains("password")
        || joined.contains("enter pass")
    {
        return reason(AwaitingInputKind::Pinentry, lines);
    }
    if joined.contains("permission")
        || joined.contains("allow")
        || joined.contains("deny")
        || joined.contains("approve")
    {
        return reason(AwaitingInputKind::Permission, lines);
    }
    if joined.contains("do you want")
        || joined.contains("are you sure")
        || joined.contains("continue?")
        || joined.contains("proceed?")
        || joined.contains("yes/no")
        || joined.contains("[y/n]")
        || joined.contains("(y/n)")
    {
        return reason(AwaitingInputKind::Confirmation, lines);
    }
    if (joined.contains("select") || joined.contains("choose"))
        && lines.iter().any(|line| numbered_option(line))
    {
        return reason(AwaitingInputKind::Choice, lines);
    }
    if lines.iter().filter(|line| numbered_option(line)).count() >= 2 {
        return reason(AwaitingInputKind::Choice, lines);
    }
    reason(AwaitingInputKind::Other, lines)
}

/// Match a tail of visible lines against the shared prompt patterns.
///
/// Returns `None` when the tail carries no prompt marker (or the marker is too
/// weak to be trustworthy).
pub(crate) fn match_prompt(lines: &[String]) -> Option<AwaitingInputReason> {
    if lines.is_empty() || !has_prompt_marker(lines) {
        return None;
    }
    Some(classify(lines))
}

/// Conservative fallback used once the PTY has been quiet: inspect the last few
/// visible lines and classify a prompt if one is present.
pub(crate) fn generic_awaiting_input(text: &str) -> Option<AwaitingInputReason> {
    match_prompt(&prompt_lines(text, 5))
}

/// opencode's permission dialog: keeps the pattern narrow so it only fires on the
/// CLI's own wording. The generic fallback covers everything else.
pub(crate) fn opencode_awaiting_input(text: &str) -> Option<AwaitingInputReason> {
    let lines = prompt_lines(text, 6);
    let joined = lines.join("\n").to_lowercase();
    // Require a permission/per-option block, not a passing mention.
    let option_lines = lines
        .iter()
        .filter(|line| {
            let line = line.to_lowercase();
            line.contains("allow once") || line.contains("allow always") || line.contains("reject")
        })
        .count();
    let is_dialog =
        (joined.contains("permission") || joined.contains("allow") || joined.contains("reject"))
            && option_lines >= 2;
    if !is_dialog {
        return None;
    }
    Some(classify(&lines))
}

/// Claude Code's confirmation/choice dialog.
pub(crate) fn claude_awaiting_input(text: &str) -> Option<AwaitingInputReason> {
    let lines = prompt_lines(text, 8);
    let joined = lines.join("\n").to_lowercase();
    let numbered = lines.iter().filter(|line| numbered_option(line)).count() >= 2;
    let is_dialog =
        joined.contains("do you want to proceed") || (joined.contains("esc to cancel") && numbered);
    if !is_dialog {
        return None;
    }
    Some(classify(&lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        prompt_lines(text, 5)
    }

    #[test]
    fn generic_matches_permission_confirm_choice_pinentry() {
        let cases = [
            (
                "Permission required: allow this tool?",
                AwaitingInputKind::Permission,
            ),
            ("Do you want to proceed?", AwaitingInputKind::Confirmation),
            (
                "Are you sure you want to continue? [y/N]",
                AwaitingInputKind::Confirmation,
            ),
            (
                "Select an option:\n  1) Keep changes\n  2) Discard changes",
                AwaitingInputKind::Choice,
            ),
            (
                "Enter passphrase for key '/home/u/.ssh/id_ed25519':",
                AwaitingInputKind::Pinentry,
            ),
            (
                "PINENTRY: enter your password:",
                AwaitingInputKind::Pinentry,
            ),
        ];
        for (text, kind) in cases {
            let matched = generic_awaiting_input(text);
            assert_eq!(
                matched.as_ref().map(|r| r.kind),
                Some(kind),
                "text: {text:?}"
            );
        }
    }

    #[test]
    fn generic_ignores_idle_composer_and_plain_output() {
        for text in [
            "Ask anything…",
            "Compiling foo v0.1.0",
            "the permission check is documented in the guide",
            "Select the repository from the list below", // no numbered options
            "Steps:",
            "Error:",
            "func main() {",
            "1. First step", // a single numbered item is not a menu
            "❯",
            "❯ Ask anything…",
            "Here's what I'll do:",
            "Compiling favetto v0.1.0",
            "Done.",
            "",
        ] {
            assert!(
                generic_awaiting_input(text).is_none(),
                "false positive for {text:?}"
            );
        }
    }

    #[test]
    fn match_prompt_requires_a_marker() {
        // A keyword alone (no confirmation token or `?`/`:` line) is not a prompt.
        assert!(match_prompt(&lines("permission denied")).is_none());
        // A bare question without a prompt verb is no longer a marker either.
        assert!(match_prompt(&lines("waiting for you?")).is_none());
        // A confirmation token classifies as `Other` when nothing else fits.
        assert_eq!(
            match_prompt(&lines("press enter")).map(|r| r.kind),
            Some(AwaitingInputKind::Other)
        );
    }

    #[test]
    fn prompt_lines_keeps_the_newest_non_empty_tail() {
        assert_eq!(
            prompt_lines("a\n\nb\nc\nd\ne\nf", 3),
            vec!["d".to_string(), "e".to_string(), "f".to_string()]
        );
    }

    #[test]
    fn agent_specific_matchers_require_their_dialog() {
        assert_eq!(
            opencode_awaiting_input("Permission required\n❯ Allow once\n  Allow always\n  Reject")
                .map(|r| r.kind),
            Some(AwaitingInputKind::Permission)
        );
        assert!(opencode_awaiting_input("let me explain the permission model").is_none());
        assert!(opencode_awaiting_input("I will allow it and reject the rest").is_none());

        assert_eq!(
            claude_awaiting_input("Do you want to proceed?\n ❯ 1. Yes\n   2. No").map(|r| r.kind),
            Some(AwaitingInputKind::Confirmation)
        );
        assert!(claude_awaiting_input("I will proceed with the change").is_none());
        assert!(claude_awaiting_input("esc to cancel").is_none());
    }
}
