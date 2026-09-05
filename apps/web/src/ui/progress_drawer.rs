//! The drawer that follows a running bulk action, one row per target.

use dependaboard_core::{BatchProgress, PrTarget, TargetProgressState};
use dioxus::prelude::*;

use crate::ui::side_panel::SidePanel;

#[component]
pub(crate) fn ProgressDrawer(progress: BatchProgress, onclose: EventHandler<()>) -> Element {
    let settled = progress.settled();
    let total = progress.targets.len();
    let percentage = if total == 0 {
        100
    } else {
        settled * 100 / total as u64
    };
    rsx! {
        SidePanel {
            class: "progress-drawer",
            eyebrow: "Batch {progress.batch_id}",
            title: rsx! { "{progress.action} progress" },
            onclose,
            div { class: "progress-summary",
                strong { "{settled}/{total}" }
                span { "{progress.succeeded} succeeded, {progress.rejected} rejected, {progress.failed} failed" }
                progress { class: "progress progress-primary", max: "100", value: "{percentage}" }
            }
            div { class: "progress-list",
                for item in &progress.targets {
                    div { class: "progress-row",
                        span { class: "progress-state {progress_class(&item.state)}" }
                        div {
                            if let Some(html_url) = progress_target_url(&item.target) {
                                a {
                                    class: "github-pr-link",
                                    href: html_url,
                                    target: "_blank",
                                    rel: "noreferrer",
                                    strong { "{item.target.owner}/{item.target.repo}#{item.target.number}" }
                                    span { class: "external-link-glyph", "↗" }
                                }
                            } else {
                                strong { "{item.target.owner}/{item.target.repo}#{item.target.number}" }
                            }
                            small { "{progress_detail(&item.state)}" }
                        }
                    }
                }
            }
        }
    }
}

fn progress_target_url(target: &PrTarget) -> Option<&str> {
    (!target.html_url.is_empty()).then_some(target.html_url.as_str())
}

fn progress_class(state: &TargetProgressState) -> &'static str {
    match state {
        TargetProgressState::Queued => "state-queued",
        TargetProgressState::Running => "state-running",
        TargetProgressState::Succeeded { .. } => "state-succeeded",
        TargetProgressState::Rejected { .. } => "state-rejected",
        TargetProgressState::Failed { .. } => "state-failed",
    }
}

fn progress_detail(state: &TargetProgressState) -> String {
    match state {
        TargetProgressState::Queued => "queued".to_owned(),
        TargetProgressState::Running => "running".to_owned(),
        TargetProgressState::Succeeded { detail } => detail.clone(),
        TargetProgressState::Rejected { reason } => reason.to_string(),
        TargetProgressState::Failed { detail } => detail.clone(),
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    #[test]
    fn batch_progress_uses_the_canonical_pull_request_url() {
        let mut target = PrTarget {
            repository_id: 7,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            number: 9,
            expected_sha: "abc123".to_owned(),
            title: "Bump serde".to_owned(),
            html_url: "https://github.example/acme/api/pull/9".to_owned(),
        };
        assert_eq!(
            progress_target_url(&target),
            Some("https://github.example/acme/api/pull/9")
        );

        target.html_url.clear();
        assert_eq!(progress_target_url(&target), None);
    }
}
