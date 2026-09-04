//! Pure display helpers: the labels, CSS classes, and relative timestamps
//! the components print.

use dependaboard_core::{CheckStatus, UpdateType, unix_seconds};

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

pub(crate) fn relative_time(timestamp: u64) -> String {
    let now = unix_seconds();
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
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn relative_time_uses_a_supported_browser_clock() {
        let now = unix_seconds();
        assert!(now > 1_577_836_800);
        assert_eq!(relative_time(now), "now");
    }
}
