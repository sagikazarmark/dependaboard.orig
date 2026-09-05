//! The drawer that follows a running bulk action, one row per target.

use dependaboard_core::{BatchProgress, TargetProgressState};
use dioxus::prelude::*;

use crate::components::button::{Button, ButtonSize};
use crate::ui::retry::can_retry;
use crate::ui::side_panel::SidePanel;

/// `retrying` says the drawer's rejected targets are being refreshed for a
/// new batch; `onretry` is asked for that.
#[component]
pub(crate) fn ProgressDrawer(
    progress: BatchProgress,
    retrying: bool,
    onretry: EventHandler<()>,
    onclose: EventHandler<()>,
) -> Element {
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
                    TargetRow {
                        owner: item.target.owner.clone(),
                        repo: item.target.repo.clone(),
                        number: item.target.number,
                        html_url: item.target.html_url.clone(),
                        state: item.state.clone(),
                    }
                }
            }
            if can_retry(&progress) {
                div { class: "progress-footer",
                    Button {
                        size: ButtonSize::Sm,
                        class: "retry-rejected-button",
                        disabled: retrying,
                        onclick: move |_| onretry.call(()),
                        if retrying { "Refreshing..." } else { "Retry rejected" }
                    }
                }
            }
        }
    }
}

/// One target of a batch: its state as a dot, the pull request as a link to
/// GitHub when its URL is known (a target recorded before URLs travelled with
/// it has none), and the state's detail. Shared by the drawer that follows a
/// running batch and the list of the batches that have run.
#[component]
pub(crate) fn TargetRow(
    owner: String,
    repo: String,
    number: u64,
    html_url: String,
    state: TargetProgressState,
) -> Element {
    rsx! {
        div { class: "progress-row",
            span { class: "progress-state {progress_class(&state)}" }
            div {
                if html_url.is_empty() {
                    strong { "{owner}/{repo}#{number}" }
                } else {
                    a {
                        class: "github-pr-link",
                        href: html_url,
                        target: "_blank",
                        rel: "noreferrer",
                        strong { "{owner}/{repo}#{number}" }
                        span { class: "external-link-glyph", "↗" }
                    }
                }
                small { "{progress_detail(&state)}" }
            }
        }
    }
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
    use dependaboard_core::{ActionOutcome, BulkActionKind, RejectReason};

    use super::*;
    use crate::ui::pr_target;
    use crate::ui::test_support::{grouped_row, half_done_merge, render, serde_row};

    fn render_drawer(progress: BatchProgress, retrying: bool) -> String {
        #[component]
        fn Fixture(progress: BatchProgress, retrying: bool) -> Element {
            rsx! {
                ProgressDrawer {
                    progress,
                    retrying,
                    onretry: move |_| {},
                    onclose: move |_| {},
                }
            }
        }
        let mut dom = VirtualDom::new_with_props(Fixture, FixtureProps { progress, retrying });
        dom.rebuild_in_place();
        dioxus::ssr::render(&dom)
    }

    /// [`half_done_merge`] with the second target rejected for `reason`.
    fn finished_with_rejection(reason: RejectReason) -> BatchProgress {
        let mut progress = half_done_merge();
        progress.record(
            &pr_target(&serde_row()).key(),
            ActionOutcome::Rejected { reason },
        );
        progress
    }

    const RETRY_BUTTON: &str = "retry-rejected-button";

    #[test]
    fn a_finished_batch_with_a_rejected_target_offers_to_retry_the_rejected_ones() {
        let html = render_drawer(finished_with_rejection(RejectReason::Forbidden), false);

        assert!(html.contains(RETRY_BUTTON), "{html}");
        assert!(html.contains(">Retry rejected<"), "{html}");
        assert!(!html.contains("disabled=true"), "{html}");
    }

    /// The refresh takes a moment per target; the button says so and cannot
    /// be pressed again until the new batch has taken the drawer over.
    #[test]
    fn while_the_rejected_targets_are_being_refreshed_the_button_says_so_and_is_disabled() {
        let html = render_drawer(finished_with_rejection(RejectReason::Forbidden), true);

        assert!(html.contains(">Refreshing...<"), "{html}");
        assert!(!html.contains(">Retry rejected<"), "{html}");
        assert!(
            html.contains(r#"retry-rejected-button" disabled=true>"#),
            "{html}"
        );
    }

    #[test]
    fn a_batch_still_running_does_not_offer_a_retry_yet() {
        // One target rejected already, but the other is still queued.
        let targets = [pr_target(&grouped_row()), pr_target(&serde_row())];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        progress.record(
            &targets[0].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::Forbidden,
            },
        );
        assert!(!progress.completed);

        let html = render_drawer(progress, false);

        assert!(!html.contains(RETRY_BUTTON), "{html}");
    }

    /// A failed target is not a rejected one: its problem was not the request
    /// but the round trip, and it is not what the retry is for.
    #[test]
    fn a_finished_batch_with_nothing_rejected_offers_no_retry() {
        let mut progress = half_done_merge();
        progress.record_failure(
            &pr_target(&serde_row()).key(),
            "GitHub mutation failed with HTTP 500: Internal Server Error",
        );

        let html = render_drawer(progress, false);

        assert!(!html.contains(RETRY_BUTTON), "{html}");
    }

    /// A pull request rejected as not found is closed or merged; there is
    /// nothing to retry it against, and the row already says so.
    #[test]
    fn a_batch_whose_only_rejections_are_not_found_offers_no_retry() {
        let html = render_drawer(finished_with_rejection(RejectReason::NotFound), false);

        assert!(!html.contains(RETRY_BUTTON), "{html}");
    }

    /// A target's row links to the pull request on GitHub when its URL is known;
    /// a target recorded before URLs travelled with it is named but not linked.
    #[test]
    fn a_target_row_links_to_the_pull_request_only_when_its_url_is_known() {
        fn Linked() -> Element {
            rsx! {
                TargetRow {
                    owner: "acme",
                    repo: "api",
                    number: 9,
                    html_url: "https://github.example/acme/api/pull/9",
                    state: TargetProgressState::Queued,
                }
            }
        }
        let linked = render(Linked);
        assert!(
            linked.contains(
                r#"<a class="github-pr-link" href="https://github.example/acme/api/pull/9" target="_blank" rel="noreferrer"><strong>acme/api#9</strong>"#
            ),
            "{linked}"
        );

        fn Unlinked() -> Element {
            rsx! {
                TargetRow {
                    owner: "acme",
                    repo: "api",
                    number: 9,
                    html_url: "",
                    state: TargetProgressState::Queued,
                }
            }
        }
        let unlinked = render(Unlinked);
        assert!(!unlinked.contains("<a "), "{unlinked}");
        assert!(
            unlinked.contains("<strong>acme/api#9</strong>"),
            "{unlinked}"
        );
    }
}
