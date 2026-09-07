//! Pure display helpers: the labels, CSS classes, and relative timestamps
//! the components print.

use dependaboard_core::{CheckStatus, UpdateType};

use crate::ui::dashboard_state::Connection;

pub(crate) fn status_label(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Success => "Passing",
        CheckStatus::Failure => "Failing",
        CheckStatus::Pending => "Pending",
        CheckStatus::None => "No checks",
    }
}

pub(crate) fn status_class(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Success => "status-success",
        CheckStatus::Failure => "status-failure",
        CheckStatus::Pending => "status-pending",
        CheckStatus::None => "status-none",
    }
}

pub(crate) fn update_class(update_type: UpdateType) -> &'static str {
    match update_type {
        UpdateType::Major => "type-major",
        UpdateType::Minor => "type-minor",
        UpdateType::Patch => "type-patch",
        UpdateType::Unknown => "type-unknown",
    }
}

pub(crate) fn version_label(from: Option<&str>, to: Option<&str>) -> String {
    match (from, to) {
        (Some(from), Some(to)) => format!("{from} -> {to}"),
        _ => "group update".to_owned(),
    }
}

/// `count` as a phrase: "1 pull request", "12 pull requests".
pub(crate) fn pull_requests(count: u64) -> String {
    match count {
        1 => "1 pull request".to_owned(),
        count => format!("{count} pull requests"),
    }
}

/// `count` as a phrase: "1 repository", "3 repositories".
pub(crate) fn repositories(count: usize) -> String {
    match count {
        1 => "1 repository".to_owned(),
        count => format!("{count} repositories"),
    }
}

/// A batch's verdicts as one tally: "95 succeeded, 5 rejected, 0 failed".
/// Every column is named, zero or not, and in this order wherever a batch's
/// tally is printed — the toast that announces one, the drawer that follows
/// one, the audit list of the ones that have run — so a rejection is never
/// mistaken for a failure or hidden behind a success, and the same batch
/// reads the same everywhere.
pub(crate) fn verdict_tally(succeeded: u64, rejected: u64, failed: u64) -> String {
    format!("{succeeded} succeeded, {rejected} rejected, {failed} failed")
}

/// The state of a checkbox that speaks for a set of things: none of them
/// selected, some, or every one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Checkbox {
    Unchecked,
    /// Some of the set is selected, not all.
    Mixed,
    Checked,
}

impl Checkbox {
    /// The box over a set of `total` things, `selected` of which are picked.
    /// An empty set is never checked: there is nothing to have selected.
    pub(crate) fn of(selected: usize, total: usize) -> Self {
        if selected == 0 {
            Self::Unchecked
        } else if selected == total {
            Self::Checked
        } else {
            Self::Mixed
        }
    }

    pub(crate) fn class(self) -> &'static str {
        match self {
            Self::Unchecked => "selection-box",
            Self::Mixed => "selection-box mixed",
            Self::Checked => "selection-box checked",
        }
    }

    pub(crate) fn mark(self) -> &'static str {
        match self {
            Self::Unchecked => "",
            Self::Mixed => "-",
            Self::Checked => "x",
        }
    }
}

/// How long before `now` the `timestamp` was, in the largest whole unit down
/// to a minute: "now", "5m", "3h", "2d", "1w". Pure in `now` so a component
/// can show the time against the dashboard's ticking clock.
pub(crate) fn relative_time(now: u64, timestamp: u64) -> String {
    let seconds = now.saturating_sub(timestamp);
    match seconds {
        0..=59 => "now".to_owned(),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        86_400..=604_799 => format!("{}d", seconds / 86_400),
        _ => format!("{}w", seconds / 604_800),
    }
}

/// [`relative_time`] as the end of a sentence: "moments ago", "5m ago". A
/// clock that ticks once a minute can stand behind an event by nearly that
/// long, so the first minute is "moments" rather than a claim of "now".
pub(crate) fn ago(now: u64, timestamp: u64) -> String {
    match relative_time(now, timestamp).as_str() {
        "now" => "moments ago".to_owned(),
        age => format!("{age} ago"),
    }
}

/// `timestamp`, Unix seconds, as the instant itself in UTC to the second:
/// "2023-11-14T22:13:20Z". ISO 8601, so it reads the same in a log, a link,
/// and a `<time>`'s `datetime`, and is valid in all three. Where a relative
/// age floors a fortnight-old batch and a three-week-old one to the same
/// "2w", this tells them apart. Done by hand: the browser bundle carries no
/// calendar library, and the proleptic Gregorian calendar from days is a
/// handful of integer operations.
pub(crate) fn utc_timestamp(timestamp: u64) -> String {
    let days = timestamp / 86_400;
    let seconds = timestamp % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3_600,
        seconds % 3_600 / 60,
        seconds % 60
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01, as (year, month,
/// day). Howard Hinnant's `civil_from_days`: the calendar is shifted so that
/// each year starts on March 1st and every 400-year era has the same 146,097
/// days, which puts the leap day at the end of the year where the arithmetic
/// need not treat it specially.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// The footer's dot for the line to the read model: green only while the
/// polls are answered.
pub(crate) fn connection_class(connection: Connection) -> &'static str {
    match connection {
        Connection::Online => "status-dot",
        Connection::Disconnected | Connection::SignedOut => "status-dot offline",
    }
}

/// The footer's word for the line to the read model.
pub(crate) fn connection_label(connection: Connection) -> &'static str {
    match connection {
        Connection::Online => "connected",
        Connection::Disconnected => "disconnected",
        Connection::SignedOut => "signed out",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pull_request_count_reads_as_a_phrase() {
        assert_eq!(pull_requests(0), "0 pull requests");
        assert_eq!(pull_requests(1), "1 pull request");
        assert_eq!(pull_requests(12), "12 pull requests");
    }

    #[test]
    fn a_repository_count_reads_as_a_phrase() {
        assert_eq!(repositories(0), "0 repositories");
        assert_eq!(repositories(1), "1 repository");
        assert_eq!(repositories(3), "3 repositories");
    }

    /// A box over a set is checked once the whole set is selected, mixed
    /// while only part of it is, and unchecked otherwise; an empty set is
    /// never "all selected".
    #[test]
    fn a_checkbox_over_a_set_is_checked_only_when_the_whole_set_is_selected() {
        assert_eq!(Checkbox::of(0, 3), Checkbox::Unchecked);
        assert_eq!(Checkbox::of(1, 3), Checkbox::Mixed);
        assert_eq!(Checkbox::of(3, 3), Checkbox::Checked);
        assert_eq!(Checkbox::of(0, 0), Checkbox::Unchecked);
        assert_eq!(Checkbox::Mixed.class(), "selection-box mixed");
        assert_eq!(Checkbox::Checked.mark(), "x");
    }

    /// An age in a sentence keeps the units of [`relative_time`], and says
    /// "moments" rather than "now" for the first minute: the sentence it goes
    /// in is about something being out of date.
    #[test]
    fn an_age_reads_as_a_phrase() {
        assert_eq!(ago(1000, 1000), "moments ago");
        assert_eq!(ago(1000, 941), "moments ago");
        assert_eq!(ago(1000, 700), "5m ago");
        assert_eq!(ago(1000, 1500), "moments ago", "a clock behind the event");
        assert_eq!(ago(3 * 86_400, 0), "3d ago");
    }

    /// The footer's dot and word for each state of the line.
    #[test]
    fn the_line_has_a_dot_and_a_word() {
        assert_eq!(connection_class(Connection::Online), "status-dot");
        assert_eq!(connection_label(Connection::Online), "connected");
        assert_eq!(
            connection_class(Connection::Disconnected),
            "status-dot offline"
        );
        assert_eq!(connection_label(Connection::Disconnected), "disconnected");
        assert_eq!(
            connection_class(Connection::SignedOut),
            "status-dot offline"
        );
        assert_eq!(connection_label(Connection::SignedOut), "signed out");
    }

    /// The batch's three terminal columns, in one order, wherever the tally
    /// is printed: the toast that announces the batch, the drawer that
    /// follows it, and the audit list read the same words.
    #[test]
    fn a_verdict_tally_names_every_column_in_the_same_words_everywhere() {
        assert_eq!(
            verdict_tally(95, 5, 0),
            "95 succeeded, 5 rejected, 0 failed"
        );
        assert_eq!(verdict_tally(0, 0, 0), "0 succeeded, 0 rejected, 0 failed");
    }

    /// The largest whole unit of time since the timestamp, down to a minute;
    /// anything closer, including a timestamp from a clock ahead of ours, is
    /// "now".
    #[test]
    fn a_relative_time_is_the_largest_whole_unit_since_the_timestamp() {
        let now = 1_700_000_000;

        assert_eq!(relative_time(now, now + 5), "now");
        assert_eq!(relative_time(now, now - 59), "now");
        assert_eq!(relative_time(now, now - 60), "1m");
        assert_eq!(relative_time(now, now - 3_599), "59m");
        assert_eq!(relative_time(now, now - 3_600), "1h");
        assert_eq!(relative_time(now, now - 86_399), "23h");
        assert_eq!(relative_time(now, now - 86_400), "1d");
        assert_eq!(relative_time(now, now - 604_799), "6d");
        assert_eq!(relative_time(now, now - 604_800), "1w");
        assert_eq!(relative_time(now, now - 3 * 604_800), "3w");
    }

    /// The instant itself, in UTC, to the second, in the one form that reads
    /// the same in a log, a link, and a `datetime` attribute; every field
    /// zero-padded, and the calendar's leap days and year ends in the right
    /// place.
    #[test]
    fn a_utc_timestamp_is_the_instant_to_the_second_in_iso_8601() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_timestamp(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(
            utc_timestamp(951_782_400),
            "2000-02-29T00:00:00Z",
            "a leap day"
        );
        assert_eq!(
            utc_timestamp(1_709_251_199),
            "2024-02-29T23:59:59Z",
            "a leap day's end"
        );
        assert_eq!(
            utc_timestamp(1_709_251_200),
            "2024-03-01T00:00:00Z",
            "the day after"
        );
        assert_eq!(
            utc_timestamp(1_704_067_199),
            "2023-12-31T23:59:59Z",
            "a year's end"
        );
        assert_eq!(
            utc_timestamp(1_704_067_200),
            "2024-01-01T00:00:00Z",
            "a year's start"
        );
        assert_eq!(
            utc_timestamp(1_041_379_205),
            "2003-01-01T00:00:05Z",
            "single digits padded"
        );
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use dependaboard_core::unix_seconds;
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::*;

    #[wasm_bindgen_test]
    fn relative_time_uses_a_supported_browser_clock() {
        let now = unix_seconds();
        assert!(now > 1_577_836_800);
        assert_eq!(relative_time(now, now), "now");
    }
}
