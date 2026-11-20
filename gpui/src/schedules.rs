//! The Schedules model: the table, the create/edit sheet and cron validation.
//!
//! Cron parsing uses `croner` with the same configuration as the daemon
//! (`Seconds::Required`, `dom_and_dow(true)`, UTC), so the next fire time the
//! sheet previews is the one the daemon will register. A five-field expression
//! is accepted as a convenience and normalized by adding a zero-seconds field.
//!
//! Everything here is free of GPUI types so the validation, the plain-language
//! hints and the confirmation copy can be unit-tested without a window.

use chrono::{DateTime, Utc};
use croner::parser::{CronParser, Seconds};
use favetto_wire::model::Schedule;
use serde_json::Value;

/// The first-fetch state of `schedules.list`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SchedulesStatus {
    /// No list has arrived yet.
    #[default]
    Loading,
    /// A list has been folded in (it may still be empty).
    Loaded,
    /// The last fetch failed; an older list stays visible.
    Failed {
        /// The daemon or transport error.
        message: String,
    },
}

/// The sheet's logical focus. The root owns keyboard focus, so this only routes
/// typed characters and renders the caret (the same model as the palette).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScheduleField {
    /// The cron expression.
    #[default]
    Cron,
    /// The catalog task picker.
    Task,
    /// The enabled toggle.
    Enabled,
}

impl ScheduleField {
    fn cycle(self, delta: isize) -> Self {
        let fields = [Self::Cron, Self::Task, Self::Enabled];
        let index = fields.iter().position(|field| *field == self).unwrap_or(0) as isize;
        fields[(index + delta).rem_euclid(fields.len() as isize) as usize]
    }
}

/// The create/edit sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleEditor {
    /// `None` for a new schedule; `Some(id)` edits in place.
    pub id: Option<String>,
    pub cron: String,
    pub task: String,
    /// Preserved from the edited schedule; new schedules start from null.
    pub input: Value,
    pub enabled: bool,
    pub focus: ScheduleField,
    /// Live validation of the cron field.
    pub cron_error: Option<String>,
    /// Live validation of the task field.
    pub task_error: Option<String>,
    /// The plain-language cadence plus the next fire time.
    pub hint: Option<String>,
    pub submitting: bool,
    /// A daemon rejection stays attached to the sheet.
    pub server_error: Option<String>,
}

impl ScheduleEditor {
    /// A new schedule with the first catalog task preselected.
    pub fn new(default_task: Option<String>) -> Self {
        let task = default_task.unwrap_or_default();
        let mut editor = Self {
            id: None,
            cron: String::new(),
            task: task.clone(),
            input: Value::Null,
            enabled: true,
            focus: ScheduleField::Cron,
            cron_error: None,
            task_error: None,
            hint: None,
            submitting: false,
            server_error: None,
        };
        editor.refresh(None);
        editor
    }

    /// The edit sheet for an existing row.
    pub fn edit(schedule: &Schedule, now: DateTime<Utc>) -> Self {
        let mut editor = Self {
            id: Some(schedule.id.clone()),
            cron: schedule.cron.clone(),
            task: schedule.task.clone(),
            input: schedule.input.clone(),
            enabled: schedule.enabled,
            focus: ScheduleField::Cron,
            cron_error: None,
            task_error: None,
            hint: None,
            submitting: false,
            server_error: None,
        };
        editor.refresh(Some(now));
        editor
    }

    /// What the sheet title and primary button say.
    pub fn is_edit(&self) -> bool {
        self.id.is_some()
    }

    /// Append a typed character to the focused text field.
    pub fn push_char(&mut self, character: char) {
        if character.is_control() {
            return;
        }
        match self.focus {
            ScheduleField::Cron => {
                self.cron.push(character);
                self.refresh(None);
            }
            ScheduleField::Task | ScheduleField::Enabled => {}
        }
    }

    /// Remove the last character of the focused text field.
    pub fn backspace(&mut self) {
        if self.focus == ScheduleField::Cron {
            self.cron.pop();
            self.refresh(None);
        }
    }

    pub fn cycle_focus(&mut self, delta: isize) {
        self.focus = self.focus.cycle(delta);
    }

    pub fn set_focus(&mut self, focus: ScheduleField) {
        self.focus = focus;
    }

    pub fn select_task(&mut self, task: &str) {
        self.task = task.to_string();
        self.task_error = None;
        self.server_error = None;
        self.focus = ScheduleField::Task;
    }

    pub fn toggle_enabled(&mut self) {
        self.enabled = !self.enabled;
        self.focus = ScheduleField::Enabled;
    }

    /// Recompute the hint and (when the user has typed something) the inline
    /// cron error. An empty new field is not an error until submit.
    pub fn refresh(&mut self, now: Option<DateTime<Utc>>) {
        let now = now.unwrap_or_else(Utc::now);
        self.hint = None;
        self.cron_error = None;
        if self.cron.trim().is_empty() {
            return;
        }
        match normalize_cron(&self.cron) {
            Ok(normalized) => {
                self.hint = Some(describe_cron(&normalized, now));
            }
            Err(error) => self.cron_error = Some(error),
        }
    }

    /// Validate every field. Returns the normalized cron, task and enabled flag
    /// when the form can be sent.
    pub fn validate(&mut self, now: DateTime<Utc>) -> Option<(String, String, bool)> {
        self.server_error = None;
        let normalized = match normalize_cron(&self.cron) {
            Ok(normalized) => {
                self.cron_error = None;
                normalized
            }
            Err(error) => {
                self.cron_error = Some(error);
                return None;
            }
        };
        if self.task.trim().is_empty() {
            self.task_error = Some("Choose a catalog task for this schedule.".to_string());
            return None;
        }
        self.task_error = None;
        if self.cron_error.is_none() {
            self.hint = Some(describe_cron(&normalized, now));
        }
        Some((normalized, self.task.trim().to_string(), self.enabled))
    }
}

/// A schedule delete awaiting explicit confirmation (docs §2, Von Restorff:
/// the destructive control is the only danger-colored one in the sheet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleDeleteConfirm {
    pub id: String,
    pub task: String,
    pub cron: String,
    /// True while `schedules.delete` is in flight.
    pub deleting: bool,
    /// A rejection stays attached to the dialog.
    pub error: Option<String>,
}

impl ScheduleDeleteConfirm {
    pub fn new(schedule: &Schedule) -> Self {
        Self {
            id: schedule.id.clone(),
            task: schedule.task.clone(),
            cron: schedule.cron.clone(),
            deleting: false,
            error: None,
        }
    }

    pub fn title(&self) -> &'static str {
        "Delete schedule"
    }

    pub fn confirm_label(&self) -> &'static str {
        if self.deleting {
            "Deleting…"
        } else {
            "Delete schedule"
        }
    }

    /// Deleting is always destructive.
    pub fn danger(&self) -> bool {
        true
    }

    pub fn body(&self) -> String {
        format!(
            "Delete the schedule '{}'? It will stop firing on the '{}' cadence; the task itself is not deleted.",
            self.task, self.cron
        )
    }
}

/// The Schedules surface model.
#[derive(Debug, Clone, Default)]
pub struct SchedulesState {
    status: SchedulesStatus,
    schedules: Vec<Schedule>,
    selected: Option<String>,
    editor: Option<ScheduleEditor>,
    confirm: Option<ScheduleDeleteConfirm>,
}

impl SchedulesState {
    pub fn status(&self) -> &SchedulesStatus {
        &self.status
    }

    pub fn schedules(&self) -> &[Schedule] {
        &self.schedules
    }

    pub fn has_entries(&self) -> bool {
        !self.schedules.is_empty()
    }

    pub fn begin_load(&mut self) {
        if self.schedules.is_empty() {
            self.status = SchedulesStatus::Loading;
        }
    }

    pub fn fail(&mut self, message: String) {
        self.status = SchedulesStatus::Failed { message };
    }

    /// Fold a fresh `schedules.list` in. Enabled schedules sort by their next
    /// fire time; disabled ones follow, so the most imminent trigger is first.
    pub fn set_entries(&mut self, schedules: Vec<Schedule>) {
        self.schedules = schedules;
        self.sort();
        self.status = SchedulesStatus::Loaded;
        self.ensure_selection();
    }

    fn ensure_selection(&mut self) {
        let still_present = self
            .selected
            .as_ref()
            .is_some_and(|id| self.schedules.iter().any(|schedule| &schedule.id == id));
        if !still_present {
            self.selected = self.schedules.first().map(|schedule| schedule.id.clone());
        }
        if let Some(confirm) = &self.confirm {
            if !self.schedules.iter().any(|s| s.id == confirm.id) {
                self.confirm = None;
            }
        }
    }

    /// Insert or replace one schedule without waiting for the next list read.
    pub fn upsert(&mut self, schedule: Schedule) {
        let id = schedule.id.clone();
        self.replace(schedule);
        self.selected = Some(id);
    }

    /// Fold an updated row and keep the sort order stable.
    pub fn replace(&mut self, schedule: Schedule) {
        if let Some(existing) = self
            .schedules
            .iter_mut()
            .find(|existing| existing.id == schedule.id)
        {
            *existing = schedule;
        } else {
            self.schedules.push(schedule);
        }
        self.sort();
        self.status = SchedulesStatus::Loaded;
    }

    fn sort(&mut self) {
        self.schedules
            .sort_by(|left, right| match (left.next_run, right.next_run) {
                (Some(a), Some(b)) => a.cmp(&b).then_with(|| left.task.cmp(&right.task)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => left
                    .task
                    .cmp(&right.task)
                    .then_with(|| left.id.cmp(&right.id)),
            });
    }

    /// Remove a deleted row.
    pub fn remove(&mut self, id: &str) {
        self.schedules.retain(|schedule| schedule.id != id);
        self.ensure_selection();
    }

    pub fn selected_id(&self) -> Option<String> {
        self.selected
            .clone()
            .filter(|id| self.schedules.iter().any(|schedule| &schedule.id == id))
            .or_else(|| self.schedules.first().map(|schedule| schedule.id.clone()))
    }

    pub fn select(&mut self, id: &str) {
        self.selected = Some(id.to_string());
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.schedules.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected_id()
            .and_then(|id| self.schedules.iter().position(|s| s.id == id))
            .unwrap_or(0);
        let last = self.schedules.len() as isize - 1;
        let next = (current as isize + delta).clamp(0, last) as usize;
        self.selected = Some(self.schedules[next].id.clone());
    }

    pub fn selection_edge(&mut self, last: bool) {
        self.selected = if last {
            self.schedules.last().map(|schedule| schedule.id.clone())
        } else {
            self.schedules.first().map(|schedule| schedule.id.clone())
        };
    }

    /// The open sheet.
    pub fn editor(&self) -> Option<&ScheduleEditor> {
        self.editor.as_ref()
    }

    pub fn editor_mut(&mut self) -> Option<&mut ScheduleEditor> {
        self.editor.as_mut()
    }

    pub fn open_editor(&mut self, editor: ScheduleEditor) {
        self.confirm = None;
        self.editor = Some(editor);
    }

    pub fn close_editor(&mut self) {
        self.editor = None;
    }

    pub fn editor_open(&self) -> bool {
        self.editor.is_some()
    }

    /// The delete dialog.
    pub fn confirm(&self) -> Option<&ScheduleDeleteConfirm> {
        self.confirm.as_ref()
    }

    pub fn open_confirm(&mut self, schedule: &Schedule) {
        self.editor = None;
        self.confirm = Some(ScheduleDeleteConfirm::new(schedule));
    }

    pub fn close_confirm(&mut self) {
        self.confirm = None;
    }

    pub fn confirm_mut(&mut self) -> Option<&mut ScheduleDeleteConfirm> {
        self.confirm.as_mut()
    }
}

/// Parse and normalize a cron expression.
///
/// Five fields are accepted as `minute hour day month weekday` and normalized
/// to the daemon's six-field form by inserting `0` seconds. Six fields are used
/// as written. The returned expression always parses with the daemon's parser.
pub fn normalize_cron(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(
            "Enter a cron expression, for example 0 0 9 * * * (every day at 09:00).".to_string(),
        );
    }
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    let normalized = match fields.len() {
        5 => format!("0 {}", fields.join(" ")),
        6 => fields.join(" "),
        count => {
            return Err(format!(
                "A cron expression has 5 or 6 space-separated fields \
                 (minute hour day month weekday, or second minute hour day month weekday); got {count}."
            ));
        }
    };
    parser().parse(&normalized).map_err(|error| {
        format!(
            "{error} Check the field ranges: seconds/minutes 0–59, hours 0–23, \
                 day of month 1–31, month 1–12, weekday 0–7 (or SUN–SAT)."
        )
    })?;
    Ok(normalized)
}

fn parser() -> CronParser {
    CronParser::builder()
        .seconds(Seconds::Required)
        .dom_and_dow(true)
        .build()
}

/// The next UTC occurrence strictly after `now`, matching the daemon's
/// `scheduler::next_run_after`.
pub fn next_run_after(normalized: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    parser()
        .parse(normalized)
        .ok()?
        .find_next_occurrence(&now, false)
        .ok()
}

/// A plain-language cadence plus the next fire time, e.g.
/// `"Every weekday at 09:00:00 UTC — next Fri 21 Nov 09:00 UTC"`.
///
/// Complex field lists fall back to `"Custom schedule"` but still carry the
/// computed next occurrence, so the hint never pretends to understand more than
/// it does.
pub fn describe_cron(normalized: &str, now: DateTime<Utc>) -> String {
    let fields: Vec<&str> = normalized.split_whitespace().collect();
    let cadence = if fields.len() == 6 {
        cadence_for(
            fields[0], fields[1], fields[2], fields[3], fields[4], fields[5],
        )
    } else {
        "Custom schedule".to_string()
    };
    match next_run_after(normalized, now) {
        Some(next) => format!("{cadence} — next {}", short_datetime(next)),
        None => format!("{cadence} — no future occurrence; check the date fields."),
    }
}

fn cadence_for(
    seconds: &str,
    minutes: &str,
    hours: &str,
    day_of_month: &str,
    month: &str,
    day_of_week: &str,
) -> String {
    let date_is_every = is_wildcard(day_of_month)
        && is_wildcard(month)
        && (is_wildcard(day_of_week) || day_of_week == "?");
    let time = time_cadence(seconds, minutes, hours);
    if date_is_every {
        return time.into();
    }

    let date = date_cadence(day_of_month, month, day_of_week);
    match time {
        TimeCadence::At(time) => format!("{date} at {time} UTC"),
        TimeCadence::Every(text) => format!("{text}, {date}"),
        TimeCadence::Custom => format!("Custom schedule ({date})"),
    }
}

enum TimeCadence {
    At(String),
    Every(String),
    Custom,
}

impl From<TimeCadence> for String {
    fn from(value: TimeCadence) -> Self {
        match value {
            TimeCadence::At(time) => format!("Every day at {time} UTC"),
            TimeCadence::Every(text) => text,
            TimeCadence::Custom => "Custom schedule".to_string(),
        }
    }
}

fn time_cadence(seconds: &str, minutes: &str, hours: &str) -> TimeCadence {
    let sec = scalar(seconds);
    let min = scalar(minutes);
    let hour = scalar(hours);

    if seconds == "*" {
        return TimeCadence::Every("Every second".to_string());
    }
    if let Some(step) = step(seconds) {
        return TimeCadence::Every(format!("Every {}", plural(step, "second")));
    }
    let Some(sec) = sec else {
        return TimeCadence::Custom;
    };

    if minutes == "*" {
        return if sec == 0 {
            TimeCadence::Every("Every minute".to_string())
        } else {
            TimeCadence::Every(format!("Every minute at second {sec}"))
        };
    }
    if let Some(step) = step(minutes) {
        return TimeCadence::Every(if sec == 0 {
            format!("Every {}", plural(step, "minute"))
        } else {
            format!("Every {} at second {sec}", plural(step, "minute"))
        });
    }
    let Some(min) = min else {
        return TimeCadence::Custom;
    };

    if hours == "*" {
        return TimeCadence::Every(format!("Every hour at minute {min:02} and second {sec:02}"));
    }
    if let Some(step) = step(hours) {
        return TimeCadence::Every(if min == 0 && sec == 0 {
            format!("Every {}", plural(step, "hour"))
        } else {
            format!(
                "Every {} at minute {min:02} and second {sec:02}",
                plural(step, "hour")
            )
        });
    }
    let Some(hour) = hour else {
        return TimeCadence::Custom;
    };
    TimeCadence::At(format!("{hour:02}:{min:02}:{sec:02}"))
}

fn date_cadence(day_of_month: &str, month: &str, day_of_week: &str) -> String {
    let weekday = if day_of_week == "1-5" {
        Some("Every weekday".to_string())
    } else if let Some(day) = scalar(day_of_week) {
        weekday_name(day).map(|name| format!("Every {name}"))
    } else {
        None
    };
    let day = scalar(day_of_month);
    let month_name = scalar(month).and_then(month_name);

    match (day, month_name, weekday) {
        (Some(day), Some(month), _) => format!("On day {day} of {month}"),
        (Some(day), None, _) => format!("On day {day} of every month"),
        (None, Some(month), _) => format!("Every day in {month}"),
        (None, None, Some(weekday)) => weekday,
        _ => "On the dates matched by the expression".to_string(),
    }
}

fn is_wildcard(field: &str) -> bool {
    field == "*" || field == "?"
}

fn scalar(field: &str) -> Option<u32> {
    field.parse::<u32>().ok()
}

fn step(field: &str) -> Option<u32> {
    let (base, step) = field.split_once('/')?;
    if base != "*" && base != "0" {
        return None;
    }
    step.parse::<u32>().ok().filter(|step| *step > 0)
}

fn plural(value: u32, unit: &str) -> String {
    if value == 1 {
        format!("1 {unit}")
    } else {
        format!("{value} {unit}s")
    }
}

fn weekday_name(day: u32) -> Option<&'static str> {
    Some(match day {
        0 | 7 => "Sunday",
        1 => "Monday",
        2 => "Tuesday",
        3 => "Wednesday",
        4 => "Thursday",
        5 => "Friday",
        6 => "Saturday",
        _ => return None,
    })
}

fn month_name(month: u32) -> Option<&'static str> {
    Some(match month {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => return None,
    })
}

/// `Fri 21 Nov 09:00` in UTC.
pub fn short_datetime(value: DateTime<Utc>) -> String {
    value.format("%a %-d %b %H:%M UTC").to_string()
}

/// The table's next-run cell: an absolute UTC time with a relative hint.
pub fn next_run_label(schedule: &Schedule, now: DateTime<Utc>) -> String {
    if !schedule.enabled {
        return "disabled".to_string();
    }
    let Some(next) = schedule.next_run else {
        return "—".to_string();
    };
    format!("{} · {}", short_datetime(next), relative(next, now))
}

fn relative(next: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (next - now).num_seconds().max(0);
    if seconds < 60 {
        format!("in {seconds}s")
    } else if seconds < 3_600 {
        format!("in {}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("in {}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
    } else {
        format!("in {}d {}h", seconds / 86_400, (seconds % 86_400) / 3_600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("rfc3339")
            .with_timezone(&Utc)
    }

    fn schedule(id: &str, cron: &str, task: &str, enabled: bool) -> Schedule {
        Schedule {
            id: id.to_string(),
            cron: cron.to_string(),
            task: task.to_string(),
            input: Value::Null,
            enabled,
            last_run: None,
            next_run: None,
        }
    }

    #[test]
    fn five_and_six_field_expressions_normalize_to_the_daemon_form() {
        assert_eq!(
            normalize_cron("0 9 * * 1-5").expect("five fields"),
            "0 0 9 * * 1-5"
        );
        assert_eq!(
            normalize_cron("  0 0 9 * * *  ").expect("six fields"),
            "0 0 9 * * *"
        );
        assert_eq!(
            normalize_cron("*/15 * * * * *").expect("steps"),
            "*/15 * * * * *"
        );
    }

    #[test]
    fn invalid_expressions_say_what_is_wrong() {
        let empty = normalize_cron("  ").expect_err("empty");
        assert!(empty.contains("Enter a cron expression"));
        let count = normalize_cron("0 9 * *").expect_err("four fields");
        assert!(count.contains("got 4"));
        let range = normalize_cron("0 0 25 * * *").expect_err("bad hour");
        assert!(range.contains("Check the field ranges"));
    }

    #[test]
    fn hints_describe_common_cadences_and_the_next_fire() {
        let now = utc("2026-11-20T08:00:00Z"); // a Friday
        let daily = describe_cron("0 0 9 * * *", now);
        assert!(daily.starts_with("Every day at 09:00:00 UTC"));
        assert!(daily.contains("next Fri 20 Nov 09:00 UTC"));

        let weekday = describe_cron("0 30 9 * * 1-5", now);
        assert!(weekday.starts_with("Every weekday at 09:30:00 UTC"));

        let every_minutes = describe_cron("0 */15 * * * *", now);
        assert_eq!(
            every_minutes,
            "Every 15 minutes — next Fri 20 Nov 08:15 UTC"
        );

        let monthly = describe_cron("0 0 9 1 * *", now);
        assert!(monthly.starts_with("On day 1 of every month at 09:00:00 UTC"));
    }

    #[test]
    fn schedule_rows_sort_by_next_run_and_keep_the_selection() {
        let mut soon = schedule("b", "0 0 9 * * *", "beta", true);
        soon.next_run = Some(utc("2026-11-21T09:00:00Z"));
        let mut later = schedule("a", "0 0 10 * * *", "alpha", true);
        later.next_run = Some(utc("2026-11-22T10:00:00Z"));
        let disabled = schedule("c", "0 0 11 * * *", "gamma", false);
        let mut state = SchedulesState::default();
        state.set_entries(vec![disabled, later, soon]);

        let order: Vec<&str> = state.schedules().iter().map(|s| s.task.as_str()).collect();
        assert_eq!(order, ["beta", "alpha", "gamma"]);
        assert_eq!(state.selected_id().as_deref(), Some("b"));

        state.select("a");
        state.set_entries(state.schedules().to_vec());
        assert_eq!(state.selected_id().as_deref(), Some("a"));
    }

    #[test]
    fn the_editor_validates_the_cron_and_the_task() {
        let now = utc("2026-11-20T08:00:00Z");
        let mut editor = ScheduleEditor::new(Some("repo:favetto/triage".to_string()));
        assert!(editor.validate(now).is_none());
        assert!(editor
            .cron_error
            .as_deref()
            .is_some_and(|message| message.contains("Enter a cron expression")));

        editor.cron = "0 0 25 * * *".to_string();
        editor.refresh(Some(now));
        assert!(editor.cron_error.is_some());

        editor.cron = "0 9 * * 1-5".to_string();
        editor.refresh(Some(now));
        assert!(editor.cron_error.is_none());
        assert!(editor
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("weekday")));

        let (cron, task, enabled) = editor.validate(now).expect("valid");
        assert_eq!(cron, "0 0 9 * * 1-5");
        assert_eq!(task, "repo:favetto/triage");
        assert!(enabled);
    }

    #[test]
    fn next_run_labels_say_disabled_and_carry_a_relative_hint() {
        let now = utc("2026-11-20T08:00:00Z");
        let mut row = schedule("s", "0 0 9 * * *", "triage", true);
        row.next_run = Some(utc("2026-11-20T10:30:00Z"));
        assert!(next_run_label(&row, now).contains("in 2h 30m"));
        row.enabled = false;
        assert_eq!(next_run_label(&row, now), "disabled");
        row.enabled = true;
        row.next_run = None;
        assert_eq!(next_run_label(&row, now), "—");
    }

    #[test]
    fn deleting_names_the_schedule_and_its_cadence() {
        let row = schedule("s", "0 0 9 * * *", "repo:favetto/triage", true);
        let confirm = ScheduleDeleteConfirm::new(&row);
        assert_eq!(confirm.title(), "Delete schedule");
        assert!(confirm.danger());
        assert!(confirm.body().contains("repo:favetto/triage"));
        assert!(confirm.body().contains("0 0 9 * * *"));
        assert!(confirm.body().contains("task itself is not deleted"));
    }
}
