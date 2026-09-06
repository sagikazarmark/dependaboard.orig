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
