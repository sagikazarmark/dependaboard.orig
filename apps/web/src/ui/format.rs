//! Pure display helpers: the labels, CSS classes, and relative timestamps
//! the components print.

use dependaboard_core::{CheckStatus, UpdateType};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pull_request_count_reads_as_a_phrase() {
        assert_eq!(pull_requests(0), "0 pull requests");
        assert_eq!(pull_requests(1), "1 pull request");
        assert_eq!(pull_requests(12), "12 pull requests");
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
