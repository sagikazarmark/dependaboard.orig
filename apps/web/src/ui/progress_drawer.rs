//! The drawer that follows a bulk action, one row per target, and says how
//! long the batch has stood still and whether the server is answering. It
//! never says the batch is lost: a target may sit for hours inside GitHub's
//! retry and rate-limit budgets, and the batch is durable in Restate for all
//! of them.

use std::time::Duration;

use dependaboard_core::TargetProgressState;
use dioxus::prelude::*;

use crate::components::button::{Button, ButtonSize};
use crate::ui::batch::Followed;
use crate::ui::format::relative_time;
use crate::ui::retry::can_retry;
use crate::ui::side_panel::SidePanel;

/// How long a batch may stand unchanged before the drawer says it is waiting.
/// A healthy GitHub call answers in seconds, and a round of three lands its
/// first within them; a minute without a change is a call being retried or a
/// rate limit being waited out. The drawer says so, and for how long, rather
/// than guess when that will end: one call's budgets run to hours — four
/// half-hour retry budgets and three rate-limit waits of up to an hour, for
/// the guard read and again for the mutation — and the batch is durable for
/// all of them.
pub(crate) const WAITING_NOTICE_AFTER: Duration = Duration::from_secs(60);

/// `followed` is the batch as the dashboard knows it, read against `now`, the
/// dashboard's clock. `retrying` says the drawer's rejected targets are being
/// refreshed for a new batch; `onretry` is asked for that.
#[component]
pub(crate) fn ProgressDrawer(
    followed: Followed,
    now: u64,
    retrying: bool,
    onretry: EventHandler<()>,
    onclose: EventHandler<()>,
) -> Element {
    let Some(progress) = &followed.progress else {
        return rsx! {
            SidePanel {
                class: "progress-drawer",
                eyebrow: "Batch {followed.batch_id}",
                title: rsx! { "Batch progress" },
                onclose,
                p { class: "progress-note", "Asking Restate where the batch stands..." }
                if let Some(trouble) = &followed.trouble {
                    p { class: "progress-note progress-trouble", "{trouble}" }
                }
            }
        };
    };
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
            if let Some(note) = waiting_note(&followed, now) {
                p { class: "progress-note", "{note}" }
            }
            if let Some(trouble) = &followed.trouble {
                p { class: "progress-note progress-trouble",
                    "{trouble}. The batch carries on in Restate; the dashboard keeps asking after it."
                }
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
            if can_retry(progress) {
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

/// What the drawer says about a batch that is not moving, if anything: nothing
/// while the batch has changed within [`WAITING_NOTICE_AFTER`], or has run to
/// the end. Past that, since when and on whom it waits: on GitHub, once
/// Restate has spoken for the batch, or on Restate to start it, while it has
/// not.
fn waiting_note(followed: &Followed, now: u64) -> Option<String> {
    if followed.completed() || followed.unchanged_for(now) < WAITING_NOTICE_AFTER {
        return None;
    }
    let standing = relative_time(now, followed.since);
    Some(if followed.heard {
        format!(
            "Waiting on GitHub for {standing}: a call is being retried, or a rate limit waited out. \
             The batch carries on in Restate."
        )
    } else {
        format!(
            "Waiting on Restate to start the batch, {standing} after it took it: its service may be \
             down or deploying, and the batch starts when it is back."
        )
    })
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
    use dependaboard_core::{ActionOutcome, BatchProgress, BulkActionKind, RejectReason};

    use super::*;
    use crate::ui::pr_target;
    use crate::ui::test_support::{
        FIXTURE_NOW, followed, grouped_row, half_done_merge, render, serde_row,
    };

    fn render_followed(followed: Followed, now: u64, retrying: bool) -> String {
        #[component]
        fn Fixture(followed: Followed, now: u64, retrying: bool) -> Element {
            rsx! {
                ProgressDrawer {
                    followed,
                    now,
                    retrying,
                    onretry: move |_| {},
                    onclose: move |_| {},
                }
            }
        }
        let mut dom = VirtualDom::new_with_props(
            Fixture,
            FixtureProps {
                followed,
                now,
                retrying,
            },
        );
        dom.rebuild_in_place();
        dioxus::ssr::render(&dom)
    }

    /// `progress` as Restate reported it at [`FIXTURE_NOW`], read at the same
    /// moment.
    fn render_drawer(progress: BatchProgress, retrying: bool) -> String {
        render_followed(followed(progress), FIXTURE_NOW, retrying)
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
        let html = render_drawer(finished_with_rejection(RejectReason::NotMergeable), false);

        assert!(html.contains(RETRY_BUTTON), "{html}");
        assert!(html.contains(">Retry rejected<"), "{html}");
        assert!(!html.contains("disabled=true"), "{html}");
    }

    /// The refresh takes a moment per target; the button says so and cannot
    /// be pressed again until the new batch has taken the drawer over.
    #[test]
    fn while_the_rejected_targets_are_being_refreshed_the_button_says_so_and_is_disabled() {
        let html = render_drawer(finished_with_rejection(RejectReason::NotMergeable), true);

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

    /// A rejection over the configuration — the identity GitHub refused, the
    /// merge method the repository disallows, the user token the deployment
    /// lacks — meets the same answer on the next attempt; a batch with only
    /// those has nothing a retry can cure.
    #[test]
    fn a_batch_whose_only_rejections_are_over_the_configuration_offers_no_retry() {
        for reason in [
            RejectReason::Forbidden,
            RejectReason::MergeMethodDisallowed,
            RejectReason::NoUserToken,
        ] {
            let html = render_drawer(finished_with_rejection(reason.clone()), false);

            assert!(!html.contains(RETRY_BUTTON), "{reason:?}: {html}");
        }
    }

    /// The opening of the drawer's note about a batch that is not moving.
    const NOTE: &str = r#"<p class="progress-note">"#;

    /// A batch that moved within the minute is a batch at work; the drawer
    /// shows its rows and says nothing more.
    #[test]
    fn a_batch_that_changed_within_the_minute_gets_no_waiting_note() {
        let html = render_followed(
            followed(half_done_merge()),
            FIXTURE_NOW + WAITING_NOTICE_AFTER.as_secs() - 1,
            false,
        );

        assert!(!html.contains(NOTE), "{html}");
    }

    /// Nothing moved for longer than a healthy call takes. The batch is not
    /// lost, and the drawer does not say so: it says on what the batch waits
    /// and for how long, and that the batch carries on.
    #[test]
    fn a_batch_standing_still_says_since_when_it_waits_on_github_and_never_that_it_is_lost() {
        let html = render_followed(followed(half_done_merge()), FIXTURE_NOW + 47 * 60, false);

        assert!(
            html.contains(
                r#"<p class="progress-note">Waiting on GitHub for 47m: a call is being retried, or a rate limit waited out. The batch carries on in Restate.</p>"#
            ),
            "{html}"
        );
        assert!(!html.to_lowercase().contains("lost"), "{html}");
    }

    /// The dashboard's own snapshot of what it queued is not Restate's word;
    /// while Restate has not given one, the wait is on Restate, not GitHub.
    #[test]
    fn a_batch_restate_has_not_started_says_it_waits_on_restate() {
        let queued = Followed::queued(
            "batch-1",
            BulkActionKind::Merge,
            &[pr_target(&grouped_row())],
            FIXTURE_NOW,
        );

        let html = render_followed(queued, FIXTURE_NOW + 3 * 60, false);

        assert!(
            html.contains("Waiting on Restate to start the batch, 3m after it took it"),
            "{html}"
        );
        assert!(!html.contains("Waiting on GitHub"), "{html}");
    }

    /// A finished batch waits on nothing, however long ago it finished.
    #[test]
    fn a_finished_batch_gets_no_waiting_note() {
        let html = render_followed(
            followed(finished_with_rejection(RejectReason::NotMergeable)),
            FIXTURE_NOW + 24 * 3600,
            false,
        );

        assert!(!html.contains(NOTE), "{html}");
    }

    /// A poll the server did not answer says nothing about the batch; the
    /// drawer passes the reason on, over the progress last heard, and says
    /// the dashboard is still asking.
    #[test]
    fn a_failing_poll_is_reported_over_the_progress_last_heard() {
        let troubled = Followed {
            trouble: Some("Restate is unavailable".to_owned()),
            ..followed(half_done_merge())
        };

        let html = render_followed(troubled, FIXTURE_NOW, false);

        assert!(
            html.contains(
                r#"<p class="progress-note progress-trouble">Restate is unavailable. The batch carries on in Restate; the dashboard keeps asking after it.</p>"#
            ),
            "{html}"
        );
        assert!(html.contains("merge progress"), "{html}");
        assert!(html.contains("1/2"), "{html}");
    }

    /// After a reload the dashboard has the id alone; until Restate answers
    /// the drawer names the batch and says it is asking.
    #[test]
    fn a_batch_followed_by_id_alone_says_it_is_asking_restate() {
        let attaching = Followed::attaching("batch-1", FIXTURE_NOW);

        let html = render_followed(attaching, FIXTURE_NOW, false);

        assert!(html.contains("Batch batch-1"), "{html}");
        assert!(
            html.contains("Asking Restate where the batch stands..."),
            "{html}"
        );
        assert!(!html.contains("progress-summary"), "{html}");
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
